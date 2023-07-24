use std::collections::HashSet;
use openai_api_rs::v1::api::Client;
use prompt_graph_core::proto2::{ChangeValue, item, ItemCore, MemoryAction, NodeWillExecute, PromptGraphNodeLoader, PromptGraphNodeMemory, SupportedEmebddingModel, SupportedVectorDatabase};
use prompt_graph_core::templates::render_template_prompt;
use std::env;
use std::io::{Cursor, Read};
use openai_api_rs::v1::embedding::EmbeddingRequest;
use futures::executor;
use prompt_graph_core::proto2::prompt_graph_node_loader::LoadFrom;
use zip;
use prompt_graph_core::proto2::serialized_value::Val;
use anyhow::Result;
use crate::executor::NodeExecutionContext;

// TODO:
//   * zipfile in message
//   * zipfile over http
//   * http - load webpage
//   * http - load json
//   * sqlite - database proxy
//   * arbitrary changes pushed by the host environment
#[tracing::instrument]
pub fn execute_node_loader(ctx: &NodeExecutionContext) -> Result<Vec<ChangeValue>> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeLoader(n),
        item_core,
        namespaces,
        ..
    } = ctx else {
        panic!("execute_node_loader: expected NodeExecutionContext with NodeLoader item");
    };

    let mut filled_values = vec![];
    let node_name = item_core.name.clone();
    match n.load_from.as_ref().unwrap() {
        LoadFrom::ZipfileBytes(bytes) => {
            let mut cursor = Cursor::new(bytes);
            let mut zip = zip::ZipArchive::new(cursor)?;
            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                if file.is_dir() {
                    continue;
                }
                if file.name().contains("__MACOSX") || file.name().contains(".DS_Store") {
                    continue;
                }
                let mut buffer = Vec::new();
                file.read_to_end(&mut buffer).expect("Failed to read file");
                let string  = String::from_utf8_lossy(&buffer);

                for output_table in namespaces.iter() {
                    let mut address = vec![output_table.clone()];
                    address.extend(file.name().split("/").flat_map(|s| s.to_string().split(".").map(|s| s.to_string()).collect::<Vec<String>>()).collect::<Vec<String>>());
                    filled_values.push(prompt_graph_core::create_change_value(
                        address,
                        Some(Val::String(string.to_string())),
                        0
                    ));
                }
            }
        }
    }
    Ok(filled_values)
}


#[cfg(test)]
mod tests {
    use std::fs::File;
    use protobuf::EnumOrUnknown;
    use indoc::indoc;
    use prompt_graph_core::proto2::prompt_graph_node_code::Source::SourceCode;
    use prompt_graph_core::proto2::{ItemCore, PromptGraphNodeCode, PromptGraphNodeCodeSourceCode, SupportedSourceCodeLanguages};
    use crate::runtime_nodes::node_loader::node::execute_node_loader;
    use super::*;

    #[test]
    fn test_exec_load_node_zip_bytes() -> Result<()> {
        // Open the file in read-only mode
        let mut file = File::open("./tests/data/files_and_dirs.zip")?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let nwe = NodeWillExecute {
            source_node: "".to_string(),
            change_values_used_in_execution: vec![],
            matched_query_index: 0
        };

        // TODO: assert this
        // dbg!(execute_node_loader(&nwe, &&PromptGraphNodeLoader {
        //     load_from: Some(LoadFrom::ZipfileBytes(buffer)),
        // }, &ItemCore {
        //     name: "test".to_string(),
        //     ..Default::default()
        // },
        //
        //     &HashSet::from(["".to_string()])
        // ).unwrap());
        Ok(())
    }
}
