extern crate protobuf;
extern crate neon_serde3;
pub mod translations;

pub use prompt_graph_core::proto2::{ChangeValue, Path, SerializedValue};
pub use prompt_graph_core::proto2::serialized_value::Val;
pub use prompt_graph_core::proto2::*;


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

