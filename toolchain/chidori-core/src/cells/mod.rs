pub mod template_cell;
pub mod code_cell;
pub mod web_cell;
pub mod llm_prompt_cell;
mod memory_cell;

use std::cmp::Ordering;
use std::collections::HashMap;
use ts_rs::TS;
use rkyv::{Archive, Deserialize, Serialize};
use crate::library::std::ai::llm::ChatModelBatch;

#[derive(
    TS,
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
#[ts(export, export_to = "package_node/types/")]
pub enum SupportedLanguage {
    PyO3,
    Starlark,
    Deno,
}

#[derive(
    TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct CodeCell {
    pub(crate) name: Option<String>,
    pub(crate) language: SupportedLanguage,
    pub(crate) source_code: String,
    pub(crate) function_invocation: Option<String>,
}


#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub enum SupportedMemoryProviders {
    InMemory,
}


#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct MemoryCell {
    pub(crate) name: Option<String>,
    pub(crate) provider: SupportedMemoryProviders,
    pub(crate) embedding_function: String,
}


#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct TemplateCell {
    pub(crate) name: Option<String>,
    pub(crate) body: String,
}

#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct WebserviceCellEndpoint {
    pub(crate) method: String,
    pub(crate) route: String,
    pub(crate) depended_function_identity: String,
    pub(crate) arg_mapping: Vec<(String, String)>,
}

#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct WebserviceCell {
    pub(crate) name: Option<String>,
    pub(crate) configuration: String,
    pub(crate) port: u16,
}


#[derive(
TS,
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
#[ts(export, export_to = "package_node/types/")]
pub(crate) struct ScheduleCell {
    pub(crate) configuration: String,
}


#[derive(
    TS,
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
#[ts(export, export_to = "package_node/types/")]
pub enum SupportedModelProviders {
    OpenAI,
}

#[derive(
    TS,
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
#[ts(export, export_to = "package_node/types/")]
pub enum LLMPromptCell {
    Chat {
        function_invocation: bool,
        configuration: HashMap<String, String>,
        name: Option<String>,
        provider: SupportedModelProviders,
        req: String,
    },
    Completion {
        req: String,
    },
    Embedding {
        function_invocation: bool,
        configuration: HashMap<String, String>,
        name: Option<String>,
        req: String,
    },
}

#[derive(
    TS,
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
#[ts(export, export_to = "package_node/types/")]
pub enum CellTypes {
    Code(CodeCell),
    Prompt(LLMPromptCell),
    Web(WebserviceCell),
    Template(TemplateCell),
    Memory(MemoryCell),
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
        CellTypes::Code(c) => &c.name,
        CellTypes::Prompt(c) => match c {
            LLMPromptCell::Chat { name, .. } => name,
            LLMPromptCell::Completion { .. } => &None,
            LLMPromptCell::Embedding { .. } => &None
        },
        CellTypes::Web(c) => &c.name,
        CellTypes::Template(c) => &c.name,
        CellTypes::Memory(c) => &c.name
    }
}
