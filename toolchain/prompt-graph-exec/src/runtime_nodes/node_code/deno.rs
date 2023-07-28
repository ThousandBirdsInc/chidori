use deno_core::serde_json::Value;
use deno_core::{FastString, JsRuntime, RuntimeOptions, serde_json, serde_v8, v8};
use prompt_graph_core::proto2::{ChangeValue, PromptGraphNodeCodeSourceCode};
use prompt_graph_core::templates::render_template_prompt;

pub fn source_code_run_deno(source_code: String, change_set: &Vec<ChangeValue>) -> Option<Value> {
    let wrapped_source_code = format!(r#"(function main() {{
        {}
    }})();"#, source_code);

    let mut runtime = JsRuntime::new(
        RuntimeOptions::default(),
    );
    // TODO: the script receives the arguments as a json payload "#state"
    let result = runtime.execute_script(
        "main.js",
        FastString::Owned(wrapped_source_code.into_boxed_str()),
    );
    match result {
        Ok(global) => {
            let scope = &mut runtime.handle_scope();
            let local = v8::Local::new(scope, global);
            let deserialized_value = serde_v8::from_v8::<serde_json::Value>(scope, local);
            return if let Ok(value) = deserialized_value {
                Some(value)
            } else {
                None
            }
        },
        Err(e) => {
            panic!("Error executing script: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use protobuf::EnumOrUnknown;
    use indoc::indoc;
    use sled::Config;
    use prompt_graph_core::proto2::prompt_graph_node_code::Source::SourceCode;
    use prompt_graph_core::proto2::{item, ItemCore, NodeWillExecute, NodeWillExecuteOnBranch, PromptGraphNodeCode, PromptGraphNodeCodeSourceCode, SupportedSourceCodeLanguages};
    use crate::executor::NodeExecutionContext;
    use crate::runtime_nodes::node_code::node::execute_node_code;
    use super::*;

    #[test]
    fn test_exec_code_node_deno_basic() {

        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();

        let mut output_filled_values: Vec<ChangeValue> = vec![];
        let node = PromptGraphNodeCode {
            source: Some(SourceCode(PromptGraphNodeCodeSourceCode {
                language: SupportedSourceCodeLanguages::Deno as i32,
                source_code: r#"
                  (function main() {
                        return {
                            "output": "hello world"
                        }
                   })();
                "#.parse().unwrap(),
                template: false,
            })),
        };

        let nwe = NodeWillExecuteOnBranch {
            branch: 0,
            counter: 0,
            custom_node_type_name: None,
            node: Some(NodeWillExecute {
                    source_node: "".to_string(),
                    change_values_used_in_execution: vec![],
                    matched_query_index: 0,
                })
        };
        let ctx = NodeExecutionContext {
            node_will_execute_on_branch: &nwe,
            item_core: &ItemCore {
                name: "".to_string(),
                queries: Default::default(),
                output: Default::default(),
                output_tables: vec![]
            },
            item: &item::Item::NodeCode(node),
            namespaces: &HashSet::from(["".to_string()]),
            template_partials: &HashMap::new(),
            tree: &tree,
        };
        execute_node_code(&ctx);
    }
}