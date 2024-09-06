use crate::sdk::entry::Chidori;
use chidori_prompt_format::extract_yaml_frontmatter_string;
use indoc::indoc;
use std::collections::HashMap;
use std::path::Path;
use serde_derive::Serialize;
use thiserror::Error;
use crate::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, SupportedLanguage, SupportedMemoryProviders, SupportedModelProviders, TemplateCell, TextRange, WebserviceCell};

#[derive(PartialEq, Serialize, Debug)]
pub struct MarkdownCodeBlock {
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
    filename: Option<Box<std::path::PathBuf>>,
    code: Option<String>,
    num_lines: usize,
    pub(crate) result: Vec<MarkdownCodeBlock>,
}

pub(crate) fn extract_code_blocks(body: &str) -> Vec<MarkdownCodeBlock> {
    let mut code_blocks = Vec::new();
    let mut start = 0;

    // Iterate over each occurrence of backticks
    while let Some(end) = body[start..].find("```") {
        start += end + 3; // Move start to the character after the closing ```

        if let Some(end_of_code) = body[start..].find("```") {
            let code = &body[start..start + end_of_code].trim();

            // Extract first line to separate tag and name
            let mut lines = code.lines();
            let first_line = lines.next().unwrap_or_default();
            let rest: String = lines.collect::<Vec<&str>>().join("\n");

            let tag_and_name: Vec<&str> = first_line.split_whitespace().collect();
            let tag = tag_and_name.get(0).cloned().unwrap_or_default().to_string();
            let name = tag_and_name.get(1).and_then(|n| n.strip_prefix('(').and_then(|n| n.strip_suffix(')'))).map(|n| n.to_string());

            // Add the code block with the text range
            code_blocks.push(MarkdownCodeBlock {
                tag,
                name,
                body: rest,
                range: TextRange {
                    start,
                    end: start + end_of_code
                },
            });

            start += end_of_code + 3; // Move start to the character after the closing ```
        } else {
            break; // No closing backticks found, exit the loop
        }
    }

    code_blocks
}


fn parse_markdown_file(filename: &Path) -> ParsedFile {
    match std::fs::read_to_string(filename) {
        Err(e) => ParsedFile {
            filename: Some(Box::new(filename.to_path_buf())),
            code: Some("".to_owned()),
            num_lines: 0,
            result: vec![],
        },
        Ok(source) => {
            let num_lines = source.lines().count();
            let result = extract_code_blocks(&source);
            ParsedFile {
                filename: Some(Box::new(filename.to_path_buf())),
                code: Some(source.to_string()),
                num_lines,
                result,
            }
        }
    }
}

pub fn load_folder(path: &Path) -> anyhow::Result<Vec<ParsedFile>> {
    let mut res = vec![];
    for entry in path.read_dir()? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        let path = entry.path();
        if metadata.is_dir() {
            res.extend(load_folder(&path)?);
        }

        if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
            let parsed_file = parse_markdown_file(&path);
            res.push(parsed_file);
        }

        if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("py") {
            let parsed_file = parse_markdown_file(&path);
            res.push(parsed_file);
        }

        if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("js") {
            let parsed_file = parse_markdown_file(&path);
            res.push(parsed_file);
        }

        if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("ts") {
            let parsed_file = parse_markdown_file(&path);
            res.push(parsed_file);
        }
    }
    Ok(res)
}

#[derive(Error, Debug)]
pub enum InterpretError {
    #[error("Failed to split frontmatter: {0}")]
    FrontmatterSplitError(String),
    #[error("Failed to deserialize YAML: {0}")]
    YamlDeserializeError(#[from] serde_yaml::Error),
    #[error("Failed to parse port number")]
    PortParseError,
}


pub fn interpret_code_block(block: &MarkdownCodeBlock) -> Result<Option<CellTypes>, InterpretError> {
    let (frontmatter, body) = chidori_prompt_format::templating::templates::split_frontmatter(&block.body)
        .map_err(|e| InterpretError::FrontmatterSplitError(e.to_string()))?;

    Ok(match block.tag.as_str() {
        "python" | "javascript" | "py" | "js" | "ts" | "typescript" => {
            let language = match block.tag.as_str() {
                "python" | "py" => SupportedLanguage::PyO3,
                "javascript" | "js" | "typescript" | "ts" => SupportedLanguage::Deno,
                _ => unreachable!(), // Given the outer match, this branch should never be reached
            };
            Some(CellTypes::Code(CodeCell {
                name: block.name.clone(),
                language,
                source_code: block.body.clone(),
                function_invocation: None,
            }, block.range.clone()))
        },
        "memory" => Some(CellTypes::Memory(MemoryCell {
            name: block.name.clone(),
            provider: SupportedMemoryProviders::InMemory,
            embedding_function: block.body.clone(),
        }, block.range.clone())),
        "embedding" => Some(CellTypes::Embedding(LLMEmbeddingCell {
            function_invocation: false,
            configuration: serde_yaml::from_str(&frontmatter)?,
            name: block.name.clone(),
            req: body,
        }, block.range.clone())),
        "prompt" => Some(CellTypes::Prompt(LLMPromptCell::Chat {
            function_invocation: false,
            configuration: serde_yaml::from_str(&frontmatter)?,
            name: block.name.clone(),
            provider: SupportedModelProviders::OpenAI,
            req: body,
        }, block.range.clone())),
        "codegen" => Some(CellTypes::CodeGen(LLMCodeGenCell {
            function_invocation: false,
            configuration: serde_yaml::from_str(&frontmatter)?,
            name: block.name.clone(),
            provider: SupportedModelProviders::OpenAI,
            req: body,
        }, block.range.clone())),
        "html" => Some(CellTypes::Template(TemplateCell {
            name: block.name.clone(),
            body: block.body.clone(),
        }, block.range.clone())),
        _ => None,
    })
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
            .flat_map(|block| interpret_code_block(block).ok())
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
}
