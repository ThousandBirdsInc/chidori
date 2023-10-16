extern crate protobuf;

use crate::proto::{ChangeValue, Path, SerializedValue};
use crate::proto::serialized_value::Val;
pub mod graph_definition;
pub mod execution_router;
pub mod utils;
pub mod proto;
pub mod build_runtime_graph;
pub mod reactivity;
pub mod time_travel;
pub mod prompt_composition;


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
