extern crate protobuf;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use graph_definition::DefinitionGraph;
use crate::proto2::{ChangeValue, ChangeValueWithCounter, File, Item, OutputType, Path, PromptGraphNodeMemory, Query, SerializedValue};
use crate::proto2::serialized_value::Val;
pub mod graph_definition;
pub mod execution_router;
pub mod utils;
pub mod templates;
pub mod proto2;
pub mod build_runtime_graph;



/// Our local server implementation is an extension of this. Implementing support for multiple
/// agent implementations to run on the same machine.
pub fn create_change_value(address: Vec<String>, val: Option<Val>, branch: u64) -> ChangeValue {
    ChangeValue{
        path: Some(Path {
            address,
        }),
        value: Some(SerializedValue {
            val,
        }),
        branch,
    }
}
