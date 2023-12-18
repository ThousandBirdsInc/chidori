// TODO: https://crates.io/crates/yaml-front-matter (for metadata)
// TODO: documentation and prompts are one and the same - it's context

use serde::Deserialize;
use yaml_front_matter::YamlFrontMatter;

#[derive(Deserialize)]
struct Metadata {
    title: String,
    description: String,
    tags: Vec<String>,
    similar_posts: Vec<String>,
    date: String,
    favorite_numbers: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_evaluation_single_node() {
        const SIMPLE_MARKDOWN_YFM: &str = r#"
---
title: 'Parsing a Markdown file metadata into a struct'
description: 'This tutorial walks you through the practice of parsing markdown files for metadata'
tags: ['markdown', 'rust', 'files', 'parsing', 'metadata']
similar_posts:
  - 'Rendering markdown'
  - 'Using Rust to render markdown'
date: '2021-09-13T03:48:00'
favorite_numbers:
    - 3.14
    - 1970
    - 12345
---


# Parsing a **Markdown** file metadata into a `struct`

> This tutorial walks you through the practice of parsing markdown files for metadata
"#;

        let result = YamlFrontMatter::parse::<Metadata>(&SIMPLE_MARKDOWN_YFM).unwrap();

        let Metadata {
            title,
            description,
            tags,
            similar_posts,
            date,
            favorite_numbers,
        } = result.metadata;

        assert_eq!(title, "Parsing a Markdown file metadata into a struct");
        assert_eq!(
            description,
            "This tutorial walks you through the practice of parsing markdown files for metadata"
        );
        assert_eq!(
            tags,
            vec!["markdown", "rust", "files", "parsing", "metadata"]
        );
        assert_eq!(
            similar_posts,
            vec!["Rendering markdown", "Using Rust to render markdown"]
        );
        assert_eq!(date, "2021-09-13T03:48:00");
        assert_eq!(favorite_numbers, vec![3.14, 1970., 12345.]);
    }
}
