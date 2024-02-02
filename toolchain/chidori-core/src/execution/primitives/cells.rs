use rkyv::{
    archived_root, check_archived_root,
    ser::{serializers::AllocSerializer, Serializer},
    Archive, Deserialize, Serialize,
};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum SupportedLanguage {
    Python,
    Starlark,
    Deno,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub(crate) struct CodeCell {
    pub(crate) language: SupportedLanguage,
    pub(crate) source_code: String,
    pub(crate) function_invocation: Option<String>,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum SupportedModelProviders {
    OpenAI,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum LLMPromptCell {
    Chat {
        path: Option<String>,
        provider: SupportedModelProviders,
        req: String,
    },
    Completion {
        req: String,
    },
    Embedding {
        req: String,
    },
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum CellTypes {
    Code(CodeCell),
    Prompt(LLMPromptCell),
}
