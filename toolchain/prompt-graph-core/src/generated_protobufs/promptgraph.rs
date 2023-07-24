#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Query {
    #[prost(string, optional, tag = "1")]
    pub query: ::core::option::Option<::prost::alloc::string::String>,
}
/// Processed version of the Query
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct QueryPaths {
    #[prost(string, tag = "1")]
    pub node: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub path: ::prost::alloc::vec::Vec<Path>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OutputType {
    #[prost(string, tag = "2")]
    pub output: ::prost::alloc::string::String,
}
/// Processed version of the OutputType
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OutputPaths {
    #[prost(string, tag = "1")]
    pub node: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub path: ::prost::alloc::vec::Vec<Path>,
}
/// Alias is a reference to another node, any value set
/// on this node will propagate for the alias as well
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphAlias {
    #[prost(string, tag = "2")]
    pub from: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub to: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphConstant {
    #[prost(message, optional, tag = "2")]
    pub value: ::core::option::Option<SerializedValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphVar {}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphOutputValue {}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeCodeSourceCode {
    #[prost(enumeration = "SupportedSourceCodeLanguages", tag = "1")]
    pub language: i32,
    #[prost(string, tag = "2")]
    pub source_code: ::prost::alloc::string::String,
    #[prost(bool, tag = "3")]
    pub template: bool,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphParameterNode {}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphMap {
    #[prost(string, tag = "4")]
    pub path: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeCode {
    #[prost(oneof = "prompt_graph_node_code::Source", tags = "6, 7, 8")]
    pub source: ::core::option::Option<prompt_graph_node_code::Source>,
}
/// Nested message and enum types in `PromptGraphNodeCode`.
pub mod prompt_graph_node_code {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Source {
        #[prost(message, tag = "6")]
        SourceCode(super::PromptGraphNodeCodeSourceCode),
        #[prost(bytes, tag = "7")]
        Zipfile(::prost::alloc::vec::Vec<u8>),
        #[prost(string, tag = "8")]
        S3Path(::prost::alloc::string::String),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeLoader {
    #[prost(oneof = "prompt_graph_node_loader::LoadFrom", tags = "1")]
    pub load_from: ::core::option::Option<prompt_graph_node_loader::LoadFrom>,
}
/// Nested message and enum types in `PromptGraphNodeLoader`.
pub mod prompt_graph_node_loader {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum LoadFrom {
        /// Load a zip file, decompress it, and make the paths available as keys
        #[prost(bytes, tag = "1")]
        ZipfileBytes(::prost::alloc::vec::Vec<u8>),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeCustom {
    #[prost(string, tag = "1")]
    pub type_name: ::prost::alloc::string::String,
}
/// TODO: we should allow the user to freely manipulate wall-clock time
/// Output value of this should just be the timestamp
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeSchedule {
    #[prost(oneof = "prompt_graph_node_schedule::Policy", tags = "1, 2, 3")]
    pub policy: ::core::option::Option<prompt_graph_node_schedule::Policy>,
}
/// Nested message and enum types in `PromptGraphNodeSchedule`.
pub mod prompt_graph_node_schedule {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Policy {
        #[prost(string, tag = "1")]
        Crontab(::prost::alloc::string::String),
        #[prost(string, tag = "2")]
        NaturalLanguage(::prost::alloc::string::String),
        #[prost(string, tag = "3")]
        EveryMs(::prost::alloc::string::String),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodePrompt {
    #[prost(string, tag = "4")]
    pub template: ::prost::alloc::string::String,
    #[prost(float, tag = "7")]
    pub temperature: f32,
    #[prost(float, tag = "8")]
    pub top_p: f32,
    #[prost(int32, tag = "9")]
    pub max_tokens: i32,
    #[prost(float, tag = "10")]
    pub presence_penalty: f32,
    #[prost(float, tag = "11")]
    pub frequency_penalty: f32,
    /// TODO: set the user token
    /// TODO: support logit bias
    #[prost(string, repeated, tag = "12")]
    pub stop: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(oneof = "prompt_graph_node_prompt::Model", tags = "5, 6")]
    pub model: ::core::option::Option<prompt_graph_node_prompt::Model>,
}
/// Nested message and enum types in `PromptGraphNodePrompt`.
pub mod prompt_graph_node_prompt {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Model {
        #[prost(enumeration = "super::SupportedChatModel", tag = "5")]
        ChatModel(i32),
        #[prost(enumeration = "super::SupportedCompletionModel", tag = "6")]
        CompletionModel(i32),
    }
}
/// TODO: this expects a selector for the query? - no its a template and you build that
/// TODO: what about the output type? pre-defined
/// TODO: what about the metadata?
/// TODO: metadata could be an independent query, or it could instead be a template too
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeMemory {
    #[prost(string, tag = "3")]
    pub collection_name: ::prost::alloc::string::String,
    #[prost(string, tag = "4")]
    pub template: ::prost::alloc::string::String,
    #[prost(enumeration = "MemoryAction", tag = "7")]
    pub action: i32,
    #[prost(oneof = "prompt_graph_node_memory::EmbeddingModel", tags = "5")]
    pub embedding_model: ::core::option::Option<
        prompt_graph_node_memory::EmbeddingModel,
    >,
    #[prost(oneof = "prompt_graph_node_memory::VectorDbProvider", tags = "6")]
    pub vector_db_provider: ::core::option::Option<
        prompt_graph_node_memory::VectorDbProvider,
    >,
}
/// Nested message and enum types in `PromptGraphNodeMemory`.
pub mod prompt_graph_node_memory {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum EmbeddingModel {
        #[prost(enumeration = "super::SupportedEmebddingModel", tag = "5")]
        Model(i32),
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum VectorDbProvider {
        #[prost(enumeration = "super::SupportedVectorDatabase", tag = "6")]
        Db(i32),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeObservation {
    #[prost(string, tag = "4")]
    pub integration: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeComponent {
    #[prost(oneof = "prompt_graph_node_component::Transclusion", tags = "4, 5, 6")]
    pub transclusion: ::core::option::Option<prompt_graph_node_component::Transclusion>,
}
/// Nested message and enum types in `PromptGraphNodeComponent`.
pub mod prompt_graph_node_component {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Transclusion {
        #[prost(message, tag = "4")]
        InlineFile(super::File),
        #[prost(bytes, tag = "5")]
        BytesReference(::prost::alloc::vec::Vec<u8>),
        #[prost(string, tag = "6")]
        S3PathReference(::prost::alloc::string::String),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeEcho {}
/// TODO: configure resolving joins
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptGraphNodeJoin {}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ItemCore {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub queries: ::prost::alloc::vec::Vec<Query>,
    #[prost(string, repeated, tag = "3")]
    pub output_tables: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(message, optional, tag = "4")]
    pub output: ::core::option::Option<OutputType>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Item {
    #[prost(message, optional, tag = "1")]
    pub core: ::core::option::Option<ItemCore>,
    #[prost(
        oneof = "item::Item",
        tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17"
    )]
    pub item: ::core::option::Option<item::Item>,
}
/// Nested message and enum types in `Item`.
pub mod item {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Item {
        #[prost(message, tag = "2")]
        Alias(super::PromptGraphAlias),
        #[prost(message, tag = "3")]
        Map(super::PromptGraphMap),
        #[prost(message, tag = "4")]
        Constant(super::PromptGraphConstant),
        #[prost(message, tag = "5")]
        Variable(super::PromptGraphVar),
        #[prost(message, tag = "6")]
        Output(super::PromptGraphOutputValue),
        /// TODO: delete above this line
        #[prost(message, tag = "7")]
        NodeCode(super::PromptGraphNodeCode),
        #[prost(message, tag = "8")]
        NodePrompt(super::PromptGraphNodePrompt),
        #[prost(message, tag = "9")]
        NodeMemory(super::PromptGraphNodeMemory),
        #[prost(message, tag = "10")]
        NodeComponent(super::PromptGraphNodeComponent),
        #[prost(message, tag = "11")]
        NodeObservation(super::PromptGraphNodeObservation),
        #[prost(message, tag = "12")]
        NodeParameter(super::PromptGraphParameterNode),
        #[prost(message, tag = "13")]
        NodeEcho(super::PromptGraphNodeEcho),
        #[prost(message, tag = "14")]
        NodeLoader(super::PromptGraphNodeLoader),
        #[prost(message, tag = "15")]
        NodeCustom(super::PromptGraphNodeCustom),
        #[prost(message, tag = "16")]
        NodeJoin(super::PromptGraphNodeJoin),
        #[prost(message, tag = "17")]
        NodeSchedule(super::PromptGraphNodeSchedule),
    }
}
/// TODO: add a flag for 'Cleaned', 'Dirty', 'Validated'
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct File {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub nodes: ::prost::alloc::vec::Vec<Item>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Path {
    #[prost(string, repeated, tag = "1")]
    pub address: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TypeDefinition {
    #[prost(oneof = "type_definition::Type", tags = "1, 2, 3, 4, 5, 6, 7")]
    pub r#type: ::core::option::Option<type_definition::Type>,
}
/// Nested message and enum types in `TypeDefinition`.
pub mod type_definition {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Type {
        #[prost(message, tag = "1")]
        Primitive(super::PrimitiveType),
        #[prost(message, tag = "2")]
        Array(::prost::alloc::boxed::Box<super::ArrayType>),
        #[prost(message, tag = "3")]
        Object(super::ObjectType),
        #[prost(message, tag = "4")]
        Union(super::UnionType),
        #[prost(message, tag = "5")]
        Intersection(super::IntersectionType),
        #[prost(message, tag = "6")]
        Optional(::prost::alloc::boxed::Box<super::OptionalType>),
        #[prost(message, tag = "7")]
        Enum(super::EnumType),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PrimitiveType {
    #[prost(oneof = "primitive_type::Primitive", tags = "1, 2, 3, 4, 5")]
    pub primitive: ::core::option::Option<primitive_type::Primitive>,
}
/// Nested message and enum types in `PrimitiveType`.
pub mod primitive_type {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Primitive {
        #[prost(bool, tag = "1")]
        IsString(bool),
        #[prost(bool, tag = "2")]
        IsNumber(bool),
        #[prost(bool, tag = "3")]
        IsBoolean(bool),
        #[prost(bool, tag = "4")]
        IsNull(bool),
        #[prost(bool, tag = "5")]
        IsUndefined(bool),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ArrayType {
    #[prost(message, optional, boxed, tag = "1")]
    pub r#type: ::core::option::Option<::prost::alloc::boxed::Box<TypeDefinition>>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ObjectType {
    #[prost(map = "string, message", tag = "1")]
    pub fields: ::std::collections::HashMap<
        ::prost::alloc::string::String,
        TypeDefinition,
    >,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UnionType {
    #[prost(message, repeated, tag = "1")]
    pub types: ::prost::alloc::vec::Vec<TypeDefinition>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct IntersectionType {
    #[prost(message, repeated, tag = "1")]
    pub types: ::prost::alloc::vec::Vec<TypeDefinition>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OptionalType {
    #[prost(message, optional, boxed, tag = "1")]
    pub r#type: ::core::option::Option<::prost::alloc::boxed::Box<TypeDefinition>>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EnumType {
    #[prost(map = "string, string", tag = "1")]
    pub values: ::std::collections::HashMap<
        ::prost::alloc::string::String,
        ::prost::alloc::string::String,
    >,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SerializedValueArray {
    #[prost(message, repeated, tag = "1")]
    pub values: ::prost::alloc::vec::Vec<SerializedValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SerializedValueObject {
    #[prost(map = "string, message", tag = "1")]
    pub values: ::std::collections::HashMap<
        ::prost::alloc::string::String,
        SerializedValue,
    >,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SerializedValue {
    #[prost(oneof = "serialized_value::Val", tags = "2, 3, 4, 5, 6, 7")]
    pub val: ::core::option::Option<serialized_value::Val>,
}
/// Nested message and enum types in `SerializedValue`.
pub mod serialized_value {
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Val {
        #[prost(float, tag = "2")]
        Float(f32),
        #[prost(int32, tag = "3")]
        Number(i32),
        #[prost(string, tag = "4")]
        String(::prost::alloc::string::String),
        #[prost(bool, tag = "5")]
        Boolean(bool),
        #[prost(message, tag = "6")]
        Array(super::SerializedValueArray),
        #[prost(message, tag = "7")]
        Object(super::SerializedValueObject),
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ChangeValue {
    #[prost(message, optional, tag = "1")]
    pub path: ::core::option::Option<Path>,
    #[prost(message, optional, tag = "2")]
    pub value: ::core::option::Option<SerializedValue>,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WrappedChangeValue {
    #[prost(uint64, tag = "3")]
    pub monotonic_counter: u64,
    #[prost(message, optional, tag = "4")]
    pub change_value: ::core::option::Option<ChangeValue>,
}
/// Computation of a node
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeWillExecute {
    #[prost(string, tag = "1")]
    pub source_node: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub change_values_used_in_execution: ::prost::alloc::vec::Vec<WrappedChangeValue>,
    #[prost(uint64, tag = "3")]
    pub matched_query_index: u64,
}
/// Group of node computations to run
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DispatchResult {
    #[prost(message, repeated, tag = "1")]
    pub operations: ::prost::alloc::vec::Vec<NodeWillExecute>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeWillExecuteOnBranch {
    #[prost(uint64, tag = "1")]
    pub branch: u64,
    #[prost(uint64, tag = "2")]
    pub counter: u64,
    #[prost(string, optional, tag = "3")]
    pub custom_node_type_name: ::core::option::Option<::prost::alloc::string::String>,
    #[prost(message, optional, tag = "4")]
    pub node: ::core::option::Option<NodeWillExecute>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ChangeValueWithCounter {
    #[prost(message, repeated, tag = "1")]
    pub filled_values: ::prost::alloc::vec::Vec<ChangeValue>,
    #[prost(uint64, repeated, tag = "2")]
    pub parent_monotonic_counters: ::prost::alloc::vec::Vec<u64>,
    #[prost(uint64, tag = "3")]
    pub monotonic_counter: u64,
    #[prost(uint64, tag = "4")]
    pub branch: u64,
    #[prost(string, tag = "5")]
    pub source_node: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CounterWithPath {
    #[prost(uint64, tag = "1")]
    pub monotonic_counter: u64,
    #[prost(message, optional, tag = "2")]
    pub path: ::core::option::Option<Path>,
}
/// Input proposals
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InputProposal {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub output: ::core::option::Option<OutputType>,
    #[prost(uint64, tag = "3")]
    pub counter: u64,
    #[prost(uint64, tag = "4")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestInputProposalResponse {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub proposal_counter: u64,
    #[prost(message, repeated, tag = "3")]
    pub changes: ::prost::alloc::vec::Vec<ChangeValue>,
    #[prost(uint64, tag = "4")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DivergentBranch {
    #[prost(uint64, tag = "1")]
    pub branch: u64,
    #[prost(uint64, tag = "2")]
    pub diverges_at_counter: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Branch {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint64, repeated, tag = "2")]
    pub source_branch_ids: ::prost::alloc::vec::Vec<u64>,
    #[prost(message, repeated, tag = "3")]
    pub divergent_branches: ::prost::alloc::vec::Vec<DivergentBranch>,
    #[prost(uint64, tag = "4")]
    pub diverges_at_counter: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Empty {}
/// This is the return value from api calls that reports the current counter and branch the operation
/// was performed on.
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(typescript_type_def::TypeDef)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutionStatus {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub monotonic_counter: u64,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileAddressedChangeValueWithCounter {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub node_name: ::prost::alloc::string::String,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
    #[prost(uint64, tag = "4")]
    pub counter: u64,
    #[prost(message, optional, tag = "5")]
    pub change: ::core::option::Option<ChangeValueWithCounter>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestOnlyId {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FilteredPollNodeWillExecuteEventsRequest {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestAtFrame {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub frame: u64,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestNewBranch {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub source_branch_id: u64,
    #[prost(uint64, tag = "3")]
    pub diverges_at_counter: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestListBranches {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListBranchesRes {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub branches: ::prost::alloc::vec::Vec<Branch>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestFileMerge {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub file: ::core::option::Option<File>,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ParquetFile {
    #[prost(bytes = "vec", tag = "1")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct QueryAtFrame {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub query: ::core::option::Option<Query>,
    #[prost(uint64, tag = "3")]
    pub frame: u64,
    #[prost(uint64, tag = "4")]
    pub branch: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct QueryAtFrameResponse {
    #[prost(message, repeated, tag = "1")]
    pub values: ::prost::alloc::vec::Vec<WrappedChangeValue>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestAckNodeWillExecuteEvent {
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    #[prost(uint64, tag = "3")]
    pub branch: u64,
    #[prost(uint64, tag = "4")]
    pub counter: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RespondPollNodeWillExecuteEvents {
    #[prost(message, repeated, tag = "1")]
    pub node_will_execute_events: ::prost::alloc::vec::Vec<NodeWillExecuteOnBranch>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PromptLibraryRecord {
    #[prost(message, optional, tag = "1")]
    pub record: ::core::option::Option<UpsertPromptLibraryRecord>,
    #[prost(uint64, tag = "3")]
    pub version_counter: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UpsertPromptLibraryRecord {
    #[prost(string, tag = "1")]
    pub template: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub name: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub id: ::prost::alloc::string::String,
    #[prost(string, optional, tag = "4")]
    pub description: ::core::option::Option<::prost::alloc::string::String>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SupportedChatModel {
    Gpt4 = 0,
    Gpt40314 = 1,
    Gpt432k = 2,
    Gpt432k0314 = 3,
    Gpt35Turbo = 4,
    Gpt35Turbo0301 = 5,
}
impl SupportedChatModel {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SupportedChatModel::Gpt4 => "GPT_4",
            SupportedChatModel::Gpt40314 => "GPT_4_0314",
            SupportedChatModel::Gpt432k => "GPT_4_32K",
            SupportedChatModel::Gpt432k0314 => "GPT_4_32K_0314",
            SupportedChatModel::Gpt35Turbo => "GPT_3_5_TURBO",
            SupportedChatModel::Gpt35Turbo0301 => "GPT_3_5_TURBO_0301",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "GPT_4" => Some(Self::Gpt4),
            "GPT_4_0314" => Some(Self::Gpt40314),
            "GPT_4_32K" => Some(Self::Gpt432k),
            "GPT_4_32K_0314" => Some(Self::Gpt432k0314),
            "GPT_3_5_TURBO" => Some(Self::Gpt35Turbo),
            "GPT_3_5_TURBO_0301" => Some(Self::Gpt35Turbo0301),
            _ => None,
        }
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SupportedCompletionModel {
    TextDavinci003 = 0,
    TextDavinci002 = 1,
    TextCurie001 = 2,
    TextBabbage001 = 3,
    TextAda00 = 4,
}
impl SupportedCompletionModel {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SupportedCompletionModel::TextDavinci003 => "TEXT_DAVINCI_003",
            SupportedCompletionModel::TextDavinci002 => "TEXT_DAVINCI_002",
            SupportedCompletionModel::TextCurie001 => "TEXT_CURIE_001",
            SupportedCompletionModel::TextBabbage001 => "TEXT_BABBAGE_001",
            SupportedCompletionModel::TextAda00 => "TEXT_ADA_00",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "TEXT_DAVINCI_003" => Some(Self::TextDavinci003),
            "TEXT_DAVINCI_002" => Some(Self::TextDavinci002),
            "TEXT_CURIE_001" => Some(Self::TextCurie001),
            "TEXT_BABBAGE_001" => Some(Self::TextBabbage001),
            "TEXT_ADA_00" => Some(Self::TextAda00),
            _ => None,
        }
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SupportedEmebddingModel {
    TextEmbeddingAda002 = 0,
    TextSearchAdaDoc001 = 1,
}
impl SupportedEmebddingModel {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SupportedEmebddingModel::TextEmbeddingAda002 => "TEXT_EMBEDDING_ADA_002",
            SupportedEmebddingModel::TextSearchAdaDoc001 => "TEXT_SEARCH_ADA_DOC_001",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "TEXT_EMBEDDING_ADA_002" => Some(Self::TextEmbeddingAda002),
            "TEXT_SEARCH_ADA_DOC_001" => Some(Self::TextSearchAdaDoc001),
            _ => None,
        }
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SupportedVectorDatabase {
    InMemory = 0,
    Chroma = 1,
    Pineconedb = 2,
    Qdrant = 3,
}
impl SupportedVectorDatabase {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SupportedVectorDatabase::InMemory => "IN_MEMORY",
            SupportedVectorDatabase::Chroma => "CHROMA",
            SupportedVectorDatabase::Pineconedb => "PINECONEDB",
            SupportedVectorDatabase::Qdrant => "QDRANT",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "IN_MEMORY" => Some(Self::InMemory),
            "CHROMA" => Some(Self::Chroma),
            "PINECONEDB" => Some(Self::Pineconedb),
            "QDRANT" => Some(Self::Qdrant),
            _ => None,
        }
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SupportedSourceCodeLanguages {
    Deno = 0,
    Starlark = 1,
}
impl SupportedSourceCodeLanguages {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SupportedSourceCodeLanguages::Deno => "DENO",
            SupportedSourceCodeLanguages::Starlark => "STARLARK",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "DENO" => Some(Self::Deno),
            "STARLARK" => Some(Self::Starlark),
            _ => None,
        }
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum MemoryAction {
    Read = 0,
    Write = 1,
    Delete = 2,
}
impl MemoryAction {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            MemoryAction::Read => "READ",
            MemoryAction::Write => "WRITE",
            MemoryAction::Delete => "DELETE",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "READ" => Some(Self::Read),
            "WRITE" => Some(Self::Write),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }
}
/// Generated client implementations.
pub mod execution_runtime_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /// API:
    #[derive(Debug, Clone)]
    pub struct ExecutionRuntimeClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl ExecutionRuntimeClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> ExecutionRuntimeClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> ExecutionRuntimeClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            ExecutionRuntimeClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn run_query(
            &mut self,
            request: impl tonic::IntoRequest<super::QueryAtFrame>,
        ) -> std::result::Result<
            tonic::Response<super::QueryAtFrameResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/RunQuery",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "RunQuery"));
            self.inner.unary(req, path, codec).await
        }
        /// * Merge a new file - if an existing file is available at the id, will merge the new file into the existing one
        pub async fn merge(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestFileMerge>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/Merge",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "Merge"));
            self.inner.unary(req, path, codec).await
        }
        /// * Get the current graph state of a file at a branch and counter position
        pub async fn current_file_state(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestOnlyId>,
        ) -> std::result::Result<tonic::Response<super::File>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/CurrentFileState",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("promptgraph.ExecutionRuntime", "CurrentFileState"),
                );
            self.inner.unary(req, path, codec).await
        }
        /// * Get the parquet history for a specific branch and Id - returns bytes
        pub async fn get_parquet_history(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestOnlyId>,
        ) -> std::result::Result<tonic::Response<super::ParquetFile>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/GetParquetHistory",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("promptgraph.ExecutionRuntime", "GetParquetHistory"),
                );
            self.inner.unary(req, path, codec).await
        }
        /// * Resume execution
        pub async fn play(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestAtFrame>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/Play",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "Play"));
            self.inner.unary(req, path, codec).await
        }
        /// * Pause execution
        pub async fn pause(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestAtFrame>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/Pause",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "Pause"));
            self.inner.unary(req, path, codec).await
        }
        /// * Split history into a separate branch
        pub async fn branch(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestNewBranch>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/Branch",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "Branch"));
            self.inner.unary(req, path, codec).await
        }
        /// * Get all branches
        pub async fn list_branches(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestListBranches>,
        ) -> std::result::Result<
            tonic::Response<super::ListBranchesRes>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/ListBranches",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("promptgraph.ExecutionRuntime", "ListBranches"));
            self.inner.unary(req, path, codec).await
        }
        /// * List all registered files
        pub async fn list_registered_graphs(
            &mut self,
            request: impl tonic::IntoRequest<super::Empty>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ExecutionStatus>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/ListRegisteredGraphs",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "ListRegisteredGraphs",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        /// * Receive a stream of input proposals <- this is a server-side stream
        pub async fn list_input_proposals(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::InputProposal>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/ListInputProposals",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("promptgraph.ExecutionRuntime", "ListInputProposals"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        /// * Push responses to input proposals (these wait for some input from a host until they're resolved) <- RPC client to server
        pub async fn respond_to_input_proposal(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestInputProposalResponse>,
        ) -> std::result::Result<tonic::Response<super::Empty>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/RespondToInputProposal",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "RespondToInputProposal",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /// * Observe the stream of execution events <- this is a server-side stream
        pub async fn list_change_events(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChangeValueWithCounter>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/ListChangeEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("promptgraph.ExecutionRuntime", "ListChangeEvents"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn list_node_will_execute_events(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::NodeWillExecuteOnBranch>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/ListNodeWillExecuteEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "ListNodeWillExecuteEvents",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        /// * Observe when the server thinks our local node implementation should execute and with what changes
        pub async fn poll_custom_node_will_execute_events(
            &mut self,
            request: impl tonic::IntoRequest<
                super::FilteredPollNodeWillExecuteEventsRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::RespondPollNodeWillExecuteEvents>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/PollCustomNodeWillExecuteEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "PollCustomNodeWillExecuteEvents",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn ack_node_will_execute_event(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestAckNodeWillExecuteEvent>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/AckNodeWillExecuteEvent",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "AckNodeWillExecuteEvent",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /// * Receive events from workers <- this is an RPC client to server, we don't need to wait for a response from the server
        pub async fn push_worker_event(
            &mut self,
            request: impl tonic::IntoRequest<super::FileAddressedChangeValueWithCounter>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/PushWorkerEvent",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("promptgraph.ExecutionRuntime", "PushWorkerEvent"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn push_template_partial(
            &mut self,
            request: impl tonic::IntoRequest<super::UpsertPromptLibraryRecord>,
        ) -> std::result::Result<
            tonic::Response<super::ExecutionStatus>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/promptgraph.ExecutionRuntime/PushTemplatePartial",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "promptgraph.ExecutionRuntime",
                        "PushTemplatePartial",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod execution_runtime_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with ExecutionRuntimeServer.
    #[async_trait]
    pub trait ExecutionRuntime: Send + Sync + 'static {
        async fn run_query(
            &self,
            request: tonic::Request<super::QueryAtFrame>,
        ) -> std::result::Result<
            tonic::Response<super::QueryAtFrameResponse>,
            tonic::Status,
        >;
        /// * Merge a new file - if an existing file is available at the id, will merge the new file into the existing one
        async fn merge(
            &self,
            request: tonic::Request<super::RequestFileMerge>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        /// * Get the current graph state of a file at a branch and counter position
        async fn current_file_state(
            &self,
            request: tonic::Request<super::RequestOnlyId>,
        ) -> std::result::Result<tonic::Response<super::File>, tonic::Status>;
        /// * Get the parquet history for a specific branch and Id - returns bytes
        async fn get_parquet_history(
            &self,
            request: tonic::Request<super::RequestOnlyId>,
        ) -> std::result::Result<tonic::Response<super::ParquetFile>, tonic::Status>;
        /// * Resume execution
        async fn play(
            &self,
            request: tonic::Request<super::RequestAtFrame>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        /// * Pause execution
        async fn pause(
            &self,
            request: tonic::Request<super::RequestAtFrame>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        /// * Split history into a separate branch
        async fn branch(
            &self,
            request: tonic::Request<super::RequestNewBranch>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        /// * Get all branches
        async fn list_branches(
            &self,
            request: tonic::Request<super::RequestListBranches>,
        ) -> std::result::Result<tonic::Response<super::ListBranchesRes>, tonic::Status>;
        /// Server streaming response type for the ListRegisteredGraphs method.
        type ListRegisteredGraphsStream: futures_core::Stream<
                Item = std::result::Result<super::ExecutionStatus, tonic::Status>,
            >
            + Send
            + 'static;
        /// * List all registered files
        async fn list_registered_graphs(
            &self,
            request: tonic::Request<super::Empty>,
        ) -> std::result::Result<
            tonic::Response<Self::ListRegisteredGraphsStream>,
            tonic::Status,
        >;
        /// Server streaming response type for the ListInputProposals method.
        type ListInputProposalsStream: futures_core::Stream<
                Item = std::result::Result<super::InputProposal, tonic::Status>,
            >
            + Send
            + 'static;
        /// * Receive a stream of input proposals <- this is a server-side stream
        async fn list_input_proposals(
            &self,
            request: tonic::Request<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<Self::ListInputProposalsStream>,
            tonic::Status,
        >;
        /// * Push responses to input proposals (these wait for some input from a host until they're resolved) <- RPC client to server
        async fn respond_to_input_proposal(
            &self,
            request: tonic::Request<super::RequestInputProposalResponse>,
        ) -> std::result::Result<tonic::Response<super::Empty>, tonic::Status>;
        /// Server streaming response type for the ListChangeEvents method.
        type ListChangeEventsStream: futures_core::Stream<
                Item = std::result::Result<super::ChangeValueWithCounter, tonic::Status>,
            >
            + Send
            + 'static;
        /// * Observe the stream of execution events <- this is a server-side stream
        async fn list_change_events(
            &self,
            request: tonic::Request<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<Self::ListChangeEventsStream>,
            tonic::Status,
        >;
        /// Server streaming response type for the ListNodeWillExecuteEvents method.
        type ListNodeWillExecuteEventsStream: futures_core::Stream<
                Item = std::result::Result<super::NodeWillExecuteOnBranch, tonic::Status>,
            >
            + Send
            + 'static;
        async fn list_node_will_execute_events(
            &self,
            request: tonic::Request<super::RequestOnlyId>,
        ) -> std::result::Result<
            tonic::Response<Self::ListNodeWillExecuteEventsStream>,
            tonic::Status,
        >;
        /// * Observe when the server thinks our local node implementation should execute and with what changes
        async fn poll_custom_node_will_execute_events(
            &self,
            request: tonic::Request<super::FilteredPollNodeWillExecuteEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RespondPollNodeWillExecuteEvents>,
            tonic::Status,
        >;
        async fn ack_node_will_execute_event(
            &self,
            request: tonic::Request<super::RequestAckNodeWillExecuteEvent>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        /// * Receive events from workers <- this is an RPC client to server, we don't need to wait for a response from the server
        async fn push_worker_event(
            &self,
            request: tonic::Request<super::FileAddressedChangeValueWithCounter>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
        async fn push_template_partial(
            &self,
            request: tonic::Request<super::UpsertPromptLibraryRecord>,
        ) -> std::result::Result<tonic::Response<super::ExecutionStatus>, tonic::Status>;
    }
    /// API:
    #[derive(Debug)]
    pub struct ExecutionRuntimeServer<T: ExecutionRuntime> {
        inner: _Inner<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    struct _Inner<T>(Arc<T>);
    impl<T: ExecutionRuntime> ExecutionRuntimeServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            let inner = _Inner(inner);
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for ExecutionRuntimeServer<T>
    where
        T: ExecutionRuntime,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/promptgraph.ExecutionRuntime/RunQuery" => {
                    #[allow(non_camel_case_types)]
                    struct RunQuerySvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::QueryAtFrame>
                    for RunQuerySvc<T> {
                        type Response = super::QueryAtFrameResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::QueryAtFrame>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move { (*inner).run_query(request).await };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = RunQuerySvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/Merge" => {
                    #[allow(non_camel_case_types)]
                    struct MergeSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestFileMerge>
                    for MergeSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestFileMerge>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move { (*inner).merge(request).await };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = MergeSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/CurrentFileState" => {
                    #[allow(non_camel_case_types)]
                    struct CurrentFileStateSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestOnlyId>
                    for CurrentFileStateSvc<T> {
                        type Response = super::File;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestOnlyId>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).current_file_state(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = CurrentFileStateSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/GetParquetHistory" => {
                    #[allow(non_camel_case_types)]
                    struct GetParquetHistorySvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestOnlyId>
                    for GetParquetHistorySvc<T> {
                        type Response = super::ParquetFile;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestOnlyId>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).get_parquet_history(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = GetParquetHistorySvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/Play" => {
                    #[allow(non_camel_case_types)]
                    struct PlaySvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestAtFrame> for PlaySvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestAtFrame>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move { (*inner).play(request).await };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PlaySvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/Pause" => {
                    #[allow(non_camel_case_types)]
                    struct PauseSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestAtFrame>
                    for PauseSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestAtFrame>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move { (*inner).pause(request).await };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PauseSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/Branch" => {
                    #[allow(non_camel_case_types)]
                    struct BranchSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestNewBranch>
                    for BranchSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestNewBranch>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move { (*inner).branch(request).await };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = BranchSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/ListBranches" => {
                    #[allow(non_camel_case_types)]
                    struct ListBranchesSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestListBranches>
                    for ListBranchesSvc<T> {
                        type Response = super::ListBranchesRes;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestListBranches>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).list_branches(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListBranchesSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/ListRegisteredGraphs" => {
                    #[allow(non_camel_case_types)]
                    struct ListRegisteredGraphsSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::ServerStreamingService<super::Empty>
                    for ListRegisteredGraphsSvc<T> {
                        type Response = super::ExecutionStatus;
                        type ResponseStream = T::ListRegisteredGraphsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::Empty>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).list_registered_graphs(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListRegisteredGraphsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/ListInputProposals" => {
                    #[allow(non_camel_case_types)]
                    struct ListInputProposalsSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::ServerStreamingService<super::RequestOnlyId>
                    for ListInputProposalsSvc<T> {
                        type Response = super::InputProposal;
                        type ResponseStream = T::ListInputProposalsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestOnlyId>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).list_input_proposals(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListInputProposalsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/RespondToInputProposal" => {
                    #[allow(non_camel_case_types)]
                    struct RespondToInputProposalSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestInputProposalResponse>
                    for RespondToInputProposalSvc<T> {
                        type Response = super::Empty;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestInputProposalResponse>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).respond_to_input_proposal(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = RespondToInputProposalSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/ListChangeEvents" => {
                    #[allow(non_camel_case_types)]
                    struct ListChangeEventsSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::ServerStreamingService<super::RequestOnlyId>
                    for ListChangeEventsSvc<T> {
                        type Response = super::ChangeValueWithCounter;
                        type ResponseStream = T::ListChangeEventsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestOnlyId>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).list_change_events(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListChangeEventsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/ListNodeWillExecuteEvents" => {
                    #[allow(non_camel_case_types)]
                    struct ListNodeWillExecuteEventsSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::ServerStreamingService<super::RequestOnlyId>
                    for ListNodeWillExecuteEventsSvc<T> {
                        type Response = super::NodeWillExecuteOnBranch;
                        type ResponseStream = T::ListNodeWillExecuteEventsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestOnlyId>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).list_node_will_execute_events(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListNodeWillExecuteEventsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/PollCustomNodeWillExecuteEvents" => {
                    #[allow(non_camel_case_types)]
                    struct PollCustomNodeWillExecuteEventsSvc<T: ExecutionRuntime>(
                        pub Arc<T>,
                    );
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<
                        super::FilteredPollNodeWillExecuteEventsRequest,
                    > for PollCustomNodeWillExecuteEventsSvc<T> {
                        type Response = super::RespondPollNodeWillExecuteEvents;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::FilteredPollNodeWillExecuteEventsRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).poll_custom_node_will_execute_events(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PollCustomNodeWillExecuteEventsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/AckNodeWillExecuteEvent" => {
                    #[allow(non_camel_case_types)]
                    struct AckNodeWillExecuteEventSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::RequestAckNodeWillExecuteEvent>
                    for AckNodeWillExecuteEventSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::RequestAckNodeWillExecuteEvent,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).ack_node_will_execute_event(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AckNodeWillExecuteEventSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/PushWorkerEvent" => {
                    #[allow(non_camel_case_types)]
                    struct PushWorkerEventSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<
                        super::FileAddressedChangeValueWithCounter,
                    > for PushWorkerEventSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::FileAddressedChangeValueWithCounter,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).push_worker_event(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PushWorkerEventSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/promptgraph.ExecutionRuntime/PushTemplatePartial" => {
                    #[allow(non_camel_case_types)]
                    struct PushTemplatePartialSvc<T: ExecutionRuntime>(pub Arc<T>);
                    impl<
                        T: ExecutionRuntime,
                    > tonic::server::UnaryService<super::UpsertPromptLibraryRecord>
                    for PushTemplatePartialSvc<T> {
                        type Response = super::ExecutionStatus;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::UpsertPromptLibraryRecord>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                (*inner).push_template_partial(request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PushTemplatePartialSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        Ok(
                            http::Response::builder()
                                .status(200)
                                .header("grpc-status", "12")
                                .header("content-type", "application/grpc")
                                .body(empty_body())
                                .unwrap(),
                        )
                    })
                }
            }
        }
    }
    impl<T: ExecutionRuntime> Clone for ExecutionRuntimeServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    impl<T: ExecutionRuntime> Clone for _Inner<T> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }
    impl<T: std::fmt::Debug> std::fmt::Debug for _Inner<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl<T: ExecutionRuntime> tonic::server::NamedService for ExecutionRuntimeServer<T> {
        const NAME: &'static str = "promptgraph.ExecutionRuntime";
    }
}
