use crate::sdk::interactive_chidori_wrapper::InteractiveChidoriWrapper;
use chidori_prompt_format::extract_yaml_frontmatter_string;
use indoc::indoc;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use serde_derive::Serialize;
use thiserror::Error;
use crate::sdk::interactive_chidori_wrapper::CellHolder;
use crate::cells::{BackingFileReference, CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, SupportedLanguage, SupportedMemoryProviders, SupportedModelProviders, TemplateCell, TextRange, WebserviceCell};

#[derive(Debug)]
pub struct TextBlock {
    pub content: String,
    pub range: TextRange,
}

#[derive(PartialEq, Serialize, Debug)]
pub struct CodeBlock {
    pub tag: String,
    pub name: Option<String>,
    pub body: String,
    pub range: TextRange,
}

enum CodeResource {
    Python,
    Js,
    Markdown
}

#[derive(Debug)]
pub struct ParsedFile {
    pub filename: Option<Box<PathBuf>>,
    pub raw_text: Option<String>,
    pub num_lines: usize,
    pub code_blocks: Vec<CodeBlock>,
    pub text_blocks: Vec<TextBlock>,
}

pub(crate) fn extract_blocks(body: &str) -> (Vec<CodeBlock>, Vec<TextBlock>) {
    let mut code_blocks = Vec::new();
    let mut text_blocks = Vec::new();
    let mut start = 0;
    let mut last_code_end = 0;

    // Iterate over each occurrence of backticks
    while let Some(code_start) = body[start..].find("```") {
        let absolute_start = start + code_start;

        // Extract text before this code block
        if absolute_start > last_code_end {
            let text = body[last_code_end..absolute_start].trim();
            if !text.is_empty() {
                text_blocks.push(TextBlock {
                    content: text.to_string(),
                    range: TextRange {
                        start: last_code_end,
                        end: absolute_start
                    },
                });
            }
        }

        start = absolute_start + 3; // Move start to after opening ```

        if let Some(end_of_code) = body[start..].find("```") {
            let code = &body[start..start + end_of_code].trim();

            // Extract first line to separate tag and name
            let mut lines = code.lines();
            let first_line = lines.next().unwrap_or_default();
            let rest: String = lines.collect::<Vec<&str>>().join("\n");

            let tag_and_name: Vec<&str> = first_line.split_whitespace().collect();
            let tag = tag_and_name.get(0).cloned().unwrap_or_default().to_string();
            let name = tag_and_name.get(1)
                .and_then(|n| n.strip_prefix('(').and_then(|n| n.strip_suffix(')')))
                .map(|n| n.to_string());

            // Add the code block
            code_blocks.push(CodeBlock {
                tag,
                name,
                body: rest,
                range: TextRange {
                    start,
                    end: start + end_of_code
                },
            });

            start += end_of_code + 3; // Move start to after closing ```
            last_code_end = start;
        } else {
            break; // No closing backticks found
        }
    }

    // Get final text block after last code block if it exists
    if last_code_end < body.len() {
        let final_text = body[last_code_end..].trim();
        if !final_text.is_empty() {
            text_blocks.push(TextBlock {
                content: final_text.to_string(),
                range: TextRange {
                    start: last_code_end,
                    end: body.len()
                },
            });
        }
    }

    (code_blocks, text_blocks)
}


pub fn parse_code_file(filename: &Path) -> ParsedFile {
    let extension = filename.extension().and_then(OsStr::to_str).unwrap();
    match std::fs::read_to_string(filename) {
        Err(_) => ParsedFile {
            filename: Some(Box::new(filename.to_path_buf())),
            raw_text: Some("".to_owned()),
            num_lines: 0,
            code_blocks: vec![],
            text_blocks: vec![],
        },
        Ok(source) => {
            let num_lines = source.lines().count();
            ParsedFile {
                filename: Some(Box::new(filename.to_path_buf())),
                raw_text: Some(source.to_string()),
                num_lines,
                code_blocks: vec![CodeBlock {
                    tag: extension.to_string(),
                    name: None,
                    body: source.to_string(),
                    range: Default::default(),
                }],
                text_blocks: vec![],
            }
        }
    }
}


pub fn parse_markdown_file(filename: &Path) -> ParsedFile {
    match std::fs::read_to_string(filename) {
        Err(_) => ParsedFile {
            filename: Some(Box::new(filename.to_path_buf())),
            raw_text: Some("".to_owned()),
            num_lines: 0,
            code_blocks: vec![],
            text_blocks: vec![],
        },
        Ok(source) => {
            let num_lines = source.lines().count();
            let (code_blocks, text_blocks) = extract_blocks(&source);
            ParsedFile {
                filename: Some(Box::new(filename.to_path_buf())),
                raw_text: Some(source.to_string()),
                num_lines,
                code_blocks,
                text_blocks,
            }
        }
    }
}

// pub fn write_folder(files: &Vec<ParsedFile>, cell_holders: &Vec<CellHolder>) {
//     for c in cell_holders {
//         if let Some(backing_file_reference ) = &c.cell.backing_file_reference() {
//             backing_file_reference.path
//         }
//     }
// }

pub fn load_folder(path: &Path) -> anyhow::Result<Vec<ParsedFile>> {
    let mut res = vec![];
    for entry in path.read_dir()? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        let path = entry.path();
        if metadata.is_dir() {
            res.extend(load_folder(&path)?);
        }

        if metadata.is_file() {
            if let Some(extension)  = path.extension().and_then(|s| s.to_str()) {
                match extension {
                    "md" => {
                        let parsed_file = parse_markdown_file(&path);
                        res.push(parsed_file);
                    },
                    "py" | "js" | "ts" => {
                        let parsed_file = parse_code_file(&path);
                        res.push(parsed_file);
                    },
                    _ => {}
                }
            }
        }
    }
    Ok(res)
}

#[derive(Error, Debug)]
pub enum InterpretError {
    #[error("Failed to split frontmatter: {0}")]
    FrontMatterSplitError(String),
    #[error("Failed to deserialize YAML: {0}")]
    YamlDeserializeError(#[from] serde_yaml::Error),
    #[error("Failed to parse port number")]
    PortParseError,
}


pub fn interpret_code_block(block: &CodeBlock, file_path: &Option<Box<PathBuf>>) -> Result<Option<CellTypes>, InterpretError> {
    let whole_body = block.body.clone();
    let (frontmatter, body) = chidori_prompt_format::templating::templates::split_frontmatter(&block.body)
        .map_err(|e| InterpretError::FrontMatterSplitError(e.to_string()))?;
    let backing_file_reference = file_path.as_ref().map(|p| BackingFileReference {
        path: p.to_string_lossy().to_string(),
        text_range: Some(block.range.clone())
    });
    Ok(match block.tag.as_str() {
        "python" | "javascript" | "py" | "js" | "ts" | "typescript" => {
            let language = match block.tag.as_str() {
                "python" | "py" => SupportedLanguage::PyO3,
                "javascript" | "js" | "typescript" | "ts" => SupportedLanguage::Deno,
                _ => unreachable!(), // Given the outer match, this branch should never be reached
            };
            Some(CellTypes::Code(CodeCell {
                backing_file_reference,
                name: block.name.clone(),
                language,
                source_code: block.body.clone(),
                function_invocation: None,
            }, block.range.clone()))
        },
        "prompt" => Some(CellTypes::Prompt(LLMPromptCell::Chat {
            backing_file_reference,
            is_function_invocation: false,
            configuration: serde_yaml::from_str(&frontmatter)?,
            name: block.name.clone(),
            provider: SupportedModelProviders::OpenAI,
            complete_body: whole_body,
            req: body,
        }, block.range.clone())),
        "codegen" => Some(CellTypes::CodeGen(LLMCodeGenCell {
            backing_file_reference,
            function_invocation: false,
            configuration: serde_yaml::from_str(&frontmatter)?,
            name: block.name.clone(),
            complete_body: whole_body,
            provider: SupportedModelProviders::OpenAI,
            req: body,
        }, block.range.clone())),
        "html" | "template" => Some(CellTypes::Template(TemplateCell {
            backing_file_reference,
            name: block.name.clone(),
            body: block.body.clone(),
        }, block.range.clone())),
        _ => None,
    })
}

pub fn cell_type_to_markdown(cell: &CellTypes) -> String {
    match cell {
        CellTypes::Code(code_cell, _) => {
            let tag = match code_cell.language {
                SupportedLanguage::PyO3 => "python",
                SupportedLanguage::Deno => "javascript",
            };
            let name_part = code_cell.name.as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            
            format!("{}{}\n{}\n", tag, name_part, code_cell.source_code)
        },
        CellTypes::Prompt(LLMPromptCell::Chat { configuration, name, req, complete_body, .. }, _) => {
            let name_part = name.as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            
            // If we have the complete_body, use it to preserve the original formatting
            if !complete_body.is_empty() {
                format!("prompt{}\n{}\n", name_part, complete_body)
            } else {
                // Otherwise reconstruct from parts
                let yaml = serde_yaml::to_string(configuration).unwrap_or_default();
                format!("prompt{}\n---\n{}\n---\n{}\n", name_part, yaml.trim(), req)
            }
        },
        CellTypes::CodeGen(code_gen, _) => {
            let name_part = code_gen.name.as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            
            if !code_gen.complete_body.is_empty() {
                format!("codegen{}\n{}\n", name_part, code_gen.complete_body)
            } else {
                let yaml = serde_yaml::to_string(&code_gen.configuration).unwrap_or_default();
                format!("codegen{}\n---\n{}\n---\n{}\n", name_part, yaml.trim(), code_gen.req)
            }
        },
        CellTypes::Template(template, _) => {
            let name_part = template.name.as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            
            format!("template{}\n{}\n", name_part, template.body)
        },
        // Add other cell types as needed
        _ => String::new()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn test_core1() {
        let contents = fs::read_to_string("./examples/core1_simple_math/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core2() {
        let contents = fs::read_to_string("./examples/core2_marshalling/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core3() {
        let contents = fs::read_to_string("./examples/core3_function_invocations/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core4() {
        let contents = fs::read_to_string("./examples/core4_async_function_invocations/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core5() {
        let contents = fs::read_to_string("./examples/core5_prompts_invoked_as_functions/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core6() {
        let contents = fs::read_to_string("./examples/core6_prompts_leveraging_function_calling/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_core7() {
        let contents = fs::read_to_string("./examples/core7_rag_stateful_memory_cells/core.md")
            .expect("Should have been able to read the file");
        let v: Vec<Option<CellTypes>> = extract_code_blocks(&contents)
            .iter()
            .flat_map(|block| interpret_code_block(block, None).ok())
            .collect();
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(v);
        });
    }

    #[test]
    fn test_extract_markdown() {
        let extracted = extract_code_blocks(indoc! {  r#"
        Generation
        
        ```python
        y = 20
        def add(a, b):
            return a + b
        ```

        ```javascript
        ---
        a: 2
        ---
        const x = add(2,2);
        ```
        
        ```prompt (multi_prompt)
        Multiply {y} times {x}
        ```

        ```html (named_html)
        <div>Example</div>
        ```
        "#
        });

        let mut map = HashMap::new();
        map.insert("a".to_string(), "2".to_string());
        insta::with_settings!({
        }, {
            insta::assert_yaml_snapshot!(extracted);
        });
    }

    #[test]
    fn test_cell_type_roundtrip() {
        let markdown = indoc! {r#"
        ```python (test_func)
        def add(a, b):
            return a + b
        ```
        "#};
        
        let (blocks, _) = extract_blocks(markdown);
        let cell = interpret_code_block(&blocks[0], None).unwrap().unwrap();
        let reconstructed = cell_type_to_markdown(&cell);
        
        assert_eq!(reconstructed.trim(), markdown.trim());
    }
}
