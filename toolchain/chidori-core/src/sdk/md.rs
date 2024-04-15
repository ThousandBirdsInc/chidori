use crate::sdk::entry::Chidori;
use chidori_prompt_format::extract_yaml_frontmatter_string;
use indoc::indoc;
use std::collections::HashMap;
use std::path::Path;
use crate::cells::{CellTypes, CodeCell, LLMPromptCell, MemoryCell, SupportedLanguage, SupportedMemoryProviders, SupportedModelProviders, TemplateCell, WebserviceCell};

#[derive(PartialEq, Debug)]
pub struct MarkdownCodeBlock {
    tag: String,
    name: Option<String>,
    configuration: HashMap<String, String>,
    body: String,
}

pub struct ParsedFile {
    // filename: Box<PathBuf>,
    // code: String,
    num_lines: usize,
    pub(crate) result: Vec<MarkdownCodeBlock>,
}

pub(crate) fn extract_code_blocks(body: &str) -> Vec<MarkdownCodeBlock> {
    let parts: Vec<&str> = body.split("```").collect();

    let mut code_blocks = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        // Code blocks are at odd indices after splitting by ```
        if index % 2 == 1 {
            code_blocks.push(*part);
        }
    }

    code_blocks
        .iter()
        .map(|m| m.trim().to_string())
        .map(|m| {
            let mut lines = m.lines();
            let first_line = lines.next().unwrap_or_default();
            let rest: String = lines.collect::<Vec<&str>>().join("\n");
            let extracted = extract_yaml_frontmatter_string(&rest);

            // Extract the name in parentheses from the first line
            let tag_and_name: Vec<&str> = first_line.split_whitespace().collect();
            let tag = tag_and_name.get(0).cloned().unwrap_or_default().to_string();
            let name = tag_and_name.get(1).and_then(|n| n.strip_prefix('(').and_then(|n| n.strip_suffix(')').and_then(|n| Some(n.to_string()))));

            MarkdownCodeBlock {
                tag,
                name,
                configuration: extracted.0,
                body: extracted.1,
            }
        })
        .collect()
}


fn parse_markdown_file(filename: &Path) -> ParsedFile {
    match std::fs::read_to_string(filename) {
        Err(e) => ParsedFile {
            // filename: Box::new(filename.to_path_buf()),
            // code: "".to_owned(),
            num_lines: 0,
            result: vec![],
        },
        Ok(source) => {
            let num_lines = source.lines().count();
            let result = extract_code_blocks(&source);
            ParsedFile {
                // filename: Box::new(filename.to_path_buf()),
                // code: source.to_string(),
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
    }
    Ok(res)
}

pub fn interpret_code_block(block: &MarkdownCodeBlock) -> Option<CellTypes> {
    match block.tag.as_str() {
        "python" | "javascript" => {
            let language = match block.tag.as_str() {
                "python" => SupportedLanguage::PyO3,
                "javascript" => SupportedLanguage::Deno,
                _ => unreachable!(), // Given the outer match, this branch should never be reached
            };
            Some(CellTypes::Code(CodeCell {
                name: block.name.clone(),
                language,
                source_code: block.body.clone(),
                function_invocation: None,
            }))
        },
        "memory" => Some(CellTypes::Memory(MemoryCell {
            name: block.name.clone(),
            provider: SupportedMemoryProviders::InMemory,
            embedding_function: block.body.clone(),
        })),
        "embedding" => Some(CellTypes::Prompt(LLMPromptCell::Embedding {
            function_invocation: false,
            configuration: block.configuration.clone(),
            name: block.name.clone(),
            req: block.body.clone(),
        })),
        "prompt" => Some(CellTypes::Prompt(LLMPromptCell::Chat {
            function_invocation: false,
            configuration: block.configuration.clone(),
            name: block.name.clone(),
            provider: SupportedModelProviders::OpenAI,
            req: block.body.clone(),
        })),
        "html" => Some(CellTypes::Template(TemplateCell {
            name: block.name.clone(),
            body: block.body.clone(),
        })),
        "web" => Some(CellTypes::Web(WebserviceCell {
            name: block.name.clone(),
            configuration: block.body.clone(),
            port: block.configuration.get("port").and_then(|p| p.parse::<u16>().ok()).or_else(|| Some(8080)).unwrap(),
        })),
        _ => None,
    }
}


#[cfg(test)]
mod test {
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use std::collections::HashMap;

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
        assert_eq!(
            extracted,
            vec![
                MarkdownCodeBlock {
                    tag: "python".to_string(),
                    name: None,
                    configuration: Default::default(),
                    body: indoc! { r#"
                y = 20
                def add(a, b):
                    return a + b"#}
                    .to_string(),
                },
                MarkdownCodeBlock {
                    tag: "javascript".to_string(),
                    name: None,
                    configuration: map,
                    body: indoc! { r#"
                    const x = add(2,2);"#}
                    .to_string(),
                },
                MarkdownCodeBlock {
                    tag: "prompt".to_string(),
                    name: Some("multi_prompt".to_string()),
                    configuration: Default::default(),
                    body: "Multiply {y} times {x}".to_string(),
                },
                MarkdownCodeBlock {
                    tag: "html".to_string(),
                    name: Some("named_html".to_string()),
                    configuration: Default::default(),
                    body: "<div>Example</div>".to_string(),
                }
            ]
        );
    }
}
