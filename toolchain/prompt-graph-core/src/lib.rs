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


#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::build_runtime_graph::graph_parse::{parse_graphql_type_def, parse_where_query};
    use crate::proto2::item::Item;

    #[test]
    fn test_parse_where_query() {
        parse_where_query("WHERE x = 1");
    }

    #[test]
    fn test_parse_graphql_type_def() {
        parse_graphql_type_def("ProductDimension", indoc! { r#"
        type ProductDimension {
          size: String
          weight: Float
        }
        "#});
    }

    #[test]
    fn test_parse_graphql_type_def_rename() {
        assert_eq!(parse_graphql_type_def("OtherName", indoc! { r#"
        type ProductDimension {
          size: String
          weight: Float
        }
        "# }).unwrap().to_string(), indoc! { r#"
        type OtherName {
          size: String
          weight: Float
        }
        "# }
        );
    }

    #[test]
    fn test_capture_resources_referred_in_graphql() {
        // capture_resources_referred_in_graphql(indoc! { r#"
        //   query GraphQuery($graph_id: ID!, $variant: String) {
        //     service(id: $graph_id) {
        //       other(filter: "example query", another: 1) {
        //         otherValue
        //       }
        //       schema(tag: $variant) {
        //         document
        //       }
        //     }
        //   }
        // "# });
    }
}


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
