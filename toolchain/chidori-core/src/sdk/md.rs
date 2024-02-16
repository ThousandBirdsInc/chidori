use crate::execution::primitives::cells::{
    CellTypes, CodeCell, LLMPromptCell, SupportedLanguage, SupportedModelProviders,
};
use crate::sdk::entry::Environment;
use chidori_prompt_format::extract_yaml_frontmatter_string;
use indoc::indoc;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(PartialEq, Debug)]
struct MarkdownCodeBlock {
    tag: String,
    configuration: HashMap<String, String>,
    body: String,
}

struct ParsedFile {
    // filename: Box<PathBuf>,
    // code: String,
    num_lines: usize,
    result: Vec<MarkdownCodeBlock>,
}

fn extract_code_blocks(body: &str) -> Vec<MarkdownCodeBlock> {
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
            MarkdownCodeBlock {
                tag: first_line.to_string(),
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

fn load_folder(path: &Path) -> anyhow::Result<Vec<ParsedFile>> {
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

fn interpret_code_block(block: &MarkdownCodeBlock) -> Option<CellTypes> {
    let is_code = match block.tag.as_str() {
        "python" | "javascript" => true,
        _ => false,
    };
    if is_code {
        let language = match block.tag.as_str() {
            "python" => SupportedLanguage::PyO3,
            "javascript" => SupportedLanguage::Deno,
            _ => unreachable!(),
        };
        return Some(CellTypes::Code(CodeCell {
            language,
            source_code: block.body.clone(),
            function_invocation: None,
        }));
    }

    let is_prompt = match block.tag.as_str() {
        "prompt" => true,
        _ => false,
    };

    if is_prompt {
        return Some(CellTypes::Prompt(LLMPromptCell::Chat {
            path: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            req: block.body.clone(),
        }));
    }

    None
}

pub fn load_md_directory(env: &mut Environment, path: &Path) -> anyhow::Result<()> {
    let files = load_folder(path)?;
    for file in files {
        for block in file.result {
            if let Some(block) = interpret_code_block(&block) {
                env.upsert_cell(block);
            }
        }
    }
    env.resolve_dependencies_from_input_signature();
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
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
        
        ```prompt
        Multiply {y} times {x}
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
                    configuration: Default::default(),
                    body: indoc! { r#"
                y = 20
                def add(a, b):
                    return a + b"#}
                    .to_string(),
                },
                MarkdownCodeBlock {
                    tag: "javascript".to_string(),
                    configuration: map,
                    body: indoc! { r#"
                    const x = add(2,2);"#}
                    .to_string(),
                },
                MarkdownCodeBlock {
                    tag: "prompt".to_string(),
                    configuration: Default::default(),
                    body: "Multiply {y} times {x}".to_string(),
                }
            ]
        );
    }

    #[test]
    fn test_load_and_eval_markdown_directory() {
        let mut env = Environment::new();
        let result = load_md_directory(&mut env, Path::new("./tests/data/markdown_graph_loader"));
        dbg!(env.step());
    }
}
