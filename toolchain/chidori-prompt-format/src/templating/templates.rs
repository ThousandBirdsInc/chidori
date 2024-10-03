///! The goal of prompt_composition is to enable the composition of prompts in a way where we can
///! trace how the final prompt was assembled and why.
use anyhow::Result;
use handlebars::template::{Parameter, Subexpression, TemplateElement, TemplateMapping};
use handlebars::{Handlebars, Path, Template};
use serde::{Deserialize, Serialize};
use serde_json::value::Map as JsonMap;
use serde_json::{Map, Value};
use std::collections::HashMap;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsValue;

// https://github.com/microsoft/guidance

// TODO: support accessing a library of prompts injected as partials
// TODO: support splitting out toml frontmatter from the template
// TODO: support async loading of partials from a remote source (callback_fn)
// TODO: expose a method for rendering at template
// TODO: expose a method for getting the required values for a template
// TODO: expose a toJSON method on the TemplateRecord object
// TODO: prompt editor should indicate what partials this expects to refer to

#[derive(Debug, Clone)]
pub struct ContextBlock {
    name: Parameter,
    params: Vec<Parameter>,
}

// TODO: to convert into a json schema we need to find the paths to each nested variable
// TODO: all dot notation elements need to convert to nested maps
// TODO: when we hit an each, that must be a list
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedVariable {
    path: Vec<BlockContextElement>,
    is_param: bool,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SchemaItemType {
    Object,
    Array,
    String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemaItem {
    pub ty: SchemaItemType,
    pub items: HashMap<String, Box<SchemaItem>>,
}

pub fn referenced_variable_list_to_schema(list: Vec<ReferencedVariable>) -> SchemaItem {
    let mut schema = SchemaItem {
        ty: SchemaItemType::Object,
        items: HashMap::new(),
    };
    for el in list {
        let mut current = &mut schema;
        // Skip params? TODO: we should skip only when they're something that introspects the param
        if el.is_param {
            continue;
        }
        for path in el.path {
            match path {
                BlockContextElement::Partial(name) | BlockContextElement::With(name) => {
                    current = current.items.entry(name).or_insert_with(|| {
                        Box::new(SchemaItem {
                            ty: SchemaItemType::Object,
                            items: HashMap::new(),
                        })
                    });
                }
                BlockContextElement::Each(name) => {
                    current = current.items.entry(name).or_insert_with(|| {
                        Box::new(SchemaItem {
                            ty: SchemaItemType::Array,
                            items: HashMap::new(),
                        })
                    });
                }
            }
        }
        current = current.items.entry(el.name).or_insert_with(|| {
            Box::new(SchemaItem {
                ty: SchemaItemType::String,
                items: Default::default(),
            })
        });
    }

    schema
}

fn extract_vars_from_param(param: &Parameter) -> Vec<String> {
    match param {
        Parameter::Literal(_) | Parameter::Name(_) => {
            // reference_paths.push(References::QueryPath(partial_context.clone(), name));
            vec![]
        }
        Parameter::Path(path) => match path {
            Path::Relative(x) => {
                vec![x.1.clone()]
            }
            Path::Local(_) => {
                vec![]
            }
        },
        Parameter::Subexpression(sexpr) => {
            if let Some(x) = &sexpr.params() {
                for param in x.iter() {
                    let param_name = extract_vars_from_param(param);
                    // reference_paths
                    //     .push(References::QueryPath(partial_context.clone(), param_name));
                }
            }
            vec![]
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BlockContextElement {
    Partial(String),
    Each(String),
    With(String),
}

pub fn analyze_referenced_partials(template: &str) -> anyhow::Result<SchemaItem> {
    let template = Template::compile(template).map_err(|e| anyhow::Error::msg(e.to_string()) )?;
    let mut reference_paths = vec![];
    analyze_referenced_partials_inner(&template, &mut reference_paths, vec![], &|s: &str| {
        Some(String::new())
    });
    Ok(referenced_variable_list_to_schema(reference_paths))
}

/// Traverse over every partial template in a Template (which can be a set of template partials) and validate that each
/// partial template can be matched to a either 1) some template type that Handlebars recognizes
/// or 2) a query path that can pull data out of the event log
pub fn analyze_referenced_partials_inner<F>(
    // TODO: wrap this and apply the conversion to a schema from the reference paths
    template: &Template,
    reference_paths: &mut Vec<ReferencedVariable>,
    block_context: Vec<BlockContextElement>,
    fetch_partial: &F,
) where
    F: Fn(&str) -> Option<String>,
{
    // TODO: produce a JSONSchema formatted properties map with type keys
    for el in &template.elements {
        let mut block_context = block_context.clone();
        match el {
            TemplateElement::RawString(_) => {}
            TemplateElement::HtmlExpression(helper_block)
            | TemplateElement::Expression(helper_block)
            | TemplateElement::HelperBlock(helper_block) => {
                let deref = *(helper_block.clone());
                let names = extract_vars_from_param(&deref.name);
                for name in names {
                    reference_paths.push(ReferencedVariable {
                        path: block_context.clone(),
                        is_param: false,
                        name: name,
                    });
                }
                for param in &deref.params {
                    let param_names = extract_vars_from_param(param);
                    for param_name in param_names {
                        if let Parameter::Name(n) = &deref.name {
                            // only some helpers create block contexts
                            match n.as_str() {
                                "each" => {
                                    block_context
                                        .push(BlockContextElement::Each(param_name.clone()));
                                }
                                "with" => {
                                    block_context
                                        .push(BlockContextElement::With(param_name.clone()));
                                }
                                _ => {}
                            }
                        }
                        reference_paths.push(ReferencedVariable {
                            path: block_context.clone(),
                            is_param: true,
                            name: param_name,
                        });
                    }
                }

                // TODO: helper params may be query paths

                // If has is a nested template
                if let Some(next_template) = deref.template {
                    analyze_referenced_partials_inner(
                        &next_template,
                        reference_paths,
                        block_context,
                        fetch_partial,
                    );
                }
            }
            TemplateElement::DecoratorExpression(decorator_block) => {}
            TemplateElement::DecoratorBlock(_) => {}
            TemplateElement::PartialExpression(x) => {
                let deref = *(x.clone());
                if let Parameter::Name(name) = deref.name {
                    if let Some(record) = fetch_partial(&name) {
                        let next_template = Template::compile(&record).unwrap();
                        block_context.push(BlockContextElement::Partial(name.clone()));
                        analyze_referenced_partials_inner(
                            &next_template,
                            reference_paths,
                            block_context,
                            fetch_partial,
                        );
                    }
                }
            }
            TemplateElement::PartialBlock(x) => {
                println!("PartialBlock {:?}", x)
            }
            TemplateElement::Comment(_) => {}
        }
    }
}

pub fn split_frontmatter(
    markdown: &str,
) -> std::result::Result<(String, String), Box<dyn std::error::Error>> {
    let mut front_matter = String::default();
    let mut sentinel = false;
    let mut front_matter_lines = 0;
    let lines: Vec<&str> = markdown.lines().collect();

    for line in &lines {
        front_matter_lines += 1;

        if line.trim() == "---" {
            if sentinel {
                // If we've encountered the second `---`, break the loop
                break;
            }
            sentinel = true; // Mark that we've encountered the first `---`
            continue;
        }

        if sentinel {
            front_matter.push_str(line);
            front_matter.push('\n');
        }
    }

    if sentinel {
        // Return front matter and the rest of the markdown if front matter was found
        Ok((
            front_matter.trim_end().to_string(), // Trim the trailing newline from the front matter
            lines[front_matter_lines..].join("\n"),
        ))
    } else {
        // Return the entire markdown as content with an empty front matter if no front matter was found
        Ok((String::default(), markdown.to_string()))
    }
}

#[wasm_bindgen]
#[derive(PartialEq, Debug, Serialize, Deserialize, Clone)]
pub enum ChatModelRoles {
    User,
    System,
    Assistant,
}

#[derive(Clone)]
pub struct TemplateWithSource {
    pub template: Template,
    pub source: String,
}

pub fn extract_roles_from_template(
    template_string: &str,
) -> Vec<(ChatModelRoles, Option<TemplateWithSource>)> {
    let temp = TemplateWithSource {
        template: Template::compile(template_string).unwrap(),
        source: template_string.to_string(),
    };
    let mut role_blocks = extract_roles_from_template_inner(&temp, vec![]);
    if role_blocks.is_empty() {
        role_blocks.push((ChatModelRoles::User, Some(temp)));
    }
    role_blocks
}

fn extract_roles_from_template_inner(
    template_with_source: &TemplateWithSource,
    context: Vec<ContextBlock>,
) -> Vec<(ChatModelRoles, Option<TemplateWithSource>)> {
    let mut role_blocks: Vec<(ChatModelRoles, Option<TemplateWithSource>)> = vec![];
    for el in &template_with_source.template.elements {
        match el {
            TemplateElement::RawString(_) => {}
            TemplateElement::HtmlExpression(helper_block)
            | TemplateElement::Expression(helper_block)
            | TemplateElement::HelperBlock(helper_block) => {
                let deref = *(helper_block.clone());
                let params = &deref.params;
                let tmpl = deref.template.clone().map(|t| {
                    let source = get_source_string_from_template(&template_with_source.source, &t);
                    TemplateWithSource {
                        template: t,
                        source,
                    }
                });
                match &deref.name {
                    Parameter::Name(name) => match name.as_str() {
                        "user" => {
                            role_blocks.push((ChatModelRoles::User, tmpl));
                        }
                        "system" => {
                            role_blocks.push((ChatModelRoles::System, tmpl));
                        }
                        "assistant" => {
                            role_blocks.push((ChatModelRoles::Assistant, tmpl));
                        }
                        _ => {}
                    },
                    Parameter::Path(path) => {}
                    Parameter::Literal(_) => {}
                    Parameter::Subexpression(_) => {}
                }
                if let Some(next_template) = deref.template {
                    let mut ctx = context.clone();
                    ctx.extend(vec![ContextBlock {
                        name: deref.name.clone(),
                        params: deref.params.clone(),
                    }]);
                    let roles = extract_roles_from_template_inner(
                        &TemplateWithSource {
                            template: next_template,
                            source: template_with_source.source.clone(),
                        },
                        ctx,
                    );
                    role_blocks.extend(roles);
                }
            }
            TemplateElement::DecoratorExpression(decorator_block) => {}
            TemplateElement::DecoratorBlock(_) => {}
            TemplateElement::PartialExpression(_) => {}
            TemplateElement::PartialBlock(_) => {}
            TemplateElement::Comment(_) => {}
        }
    }
    role_blocks
}

/// Merge two JSON maps together, where the second map takes precedence over the first
fn merge(a: &mut Value, b: Value) {
    if let Value::Object(a) = a {
        if let Value::Object(b) = b {
            for (k, v) in b {
                if v.is_null() {
                    a.remove(&k);
                } else {
                    merge(a.entry(k).or_insert(Value::Null), v);
                }
            }

            return;
        }
    }

    *a = b;
}

// TODO: remove these unwraps
// TODO: add an argument for passing a set of partials
// TODO: implement block helpers for User and System prompts

#[derive(Serialize, Deserialize)]
pub struct PromptLibraryRecord {
    template: String,
    name: String,
    id: String,
    description: Option<String>,
}

/// Render a template string, placing in partials (names that map to prompts in the prompt library) and values from the query paths
/// as records of changes that are made to the event log
pub fn render_template_prompt(
    template_str: &str,
    json_value: &serde_json::Value,
    partials: &HashMap<String, PromptLibraryRecord>,
) -> Result<String> {
    let mut reg = Handlebars::new();
    for (name, prompt) in partials.iter() {
        reg.register_partial(name, prompt.template.as_str())
            .unwrap();
    }
    reg.register_template_string("tpl_1", template_str).unwrap();
    reg.register_escape_fn(handlebars::no_escape);
    let render = reg.render("tpl_1", &json_value).unwrap();
    Ok(render)
}

fn get_source_string_from_template(source: &str, template: &Template) -> String {
    let start_index = template.span.0;
    let end_index = template.span.1;
    source[start_index..end_index].to_string()
}

/// Apply all analysis to template
fn analyze_template(source: &str) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    // use gluesql::core::ast_builder::extract;
    use indoc::indoc;
    use serde_json::json;

    // #[test]
    // fn test_template_validation() {
    //     validate_template(
    //         "Hello, {{name}}! {{user.name}}",
    //         vec![vec!["user".to_string(), "name".to_string()]],
    //     );
    // }
    //
    // #[test]
    // fn test_template_validation_eval_context() {
    //     validate_template(
    //         "{{#with user}} {{name}} {{/with}}",
    //         vec![vec!["user".to_string(), "name".to_string()]],
    //     );
    // }
    //
    // #[test]
    // fn test_template_validation_eval_context_each() {
    //     validate_template(
    //         "{{#each users}} {{name}} {{/each}}",
    //         vec![vec!["user".to_string(), "name".to_string()]],
    //     );
    // }

    #[test]
    fn test_rendering_template() {
        let value = json! {
            {
                "user": {
                    "name": "FirstName",
                    "last_name": "LastName"
                }
            }
        };

        let rendered =
            render_template_prompt(&"Basic template {{user.name}}", &value, &HashMap::new());
        assert_eq!(rendered.unwrap(), "Basic template FirstName");
    }

    #[test]
    fn test_rendering_template_with_roles() {
        let value = json! {
            {
                "value": "Testing"
            }
        };

        let roles = extract_roles_from_template(
            &r#"
{{#system}}You are a helpful assistant.{{value}}{{/system}}
{{#user}}test{{/user}}
{{#assistant}}test{{/assistant}}
"#,
        );
        let rendered: Vec<_> = roles
            .into_iter()
            .map(|(role, template)| {
                (
                    role,
                    template.map(|t| {
                        render_template_prompt(&t.source, &value, &HashMap::new()).unwrap()
                    }),
                )
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                (
                    ChatModelRoles::System,
                    Some("You are a helpful assistant.Testing".to_string())
                ),
                (ChatModelRoles::User, Some("test".to_string())),
                (ChatModelRoles::Assistant, Some("test".to_string())),
            ]
        );
    }

    #[test]
    fn test_rendering_template_with_partial() {
        let mut partials = HashMap::new();
        partials.insert(
            "part".to_string(),
            PromptLibraryRecord {
                template: "[{{user.name}} inside partial]".to_string(),
                name: "part".to_string(),
                id: "0".to_string(),
                description: None,
            },
        );

        let value = json! {
            {
                "user": {
                    "name": "FirstName",
                    "last_name": "LastName"
                }
            }
        };

        let rendered = render_template_prompt(&"Basic template {{> part}}", &value, &partials);
        assert_eq!(
            rendered.unwrap(),
            "Basic template [FirstName inside partial]"
        );
    }
    #[test]
    fn test_extraction_of_variable_references() {
        let template = "Basic template {{var}} {{dot.notation}}";
        let schema = analyze_referenced_partials(&template);
        // TODO: when a partial is used to render something, we should note it
        // TODO: we should list all variables used
        assert_eq!(
            schema,
            SchemaItem {
                ty: SchemaItemType::Object,
                items: HashMap::from([
                    (
                        "dot.notation".to_string(),
                        Box::new(SchemaItem {
                            ty: SchemaItemType::String,
                            items: HashMap::new(),
                        })
                    ),
                    (
                        "var".to_string(),
                        Box::new(SchemaItem {
                            ty: SchemaItemType::String,
                            items: HashMap::new(),
                        })
                    ),
                ]),
            }
        );
        // TODO: Add support for tracing partials used in the template
    }

    #[test]
    fn test_extraction_of_variable_references_in_helpers() {
        let template = r#"
{{#each paragraphs}}
<p>{{this}}</p>
{{else}}
<p class="empty">No content</p>
{{/each}}
{{#if author}}
<h1>{{firstName}} {{lastName}}</h1>
{{else}}
<h1>Unknown Author</h1>
{{/if}}
{{#each deeply}}
{{#if nested}}
<h1>{{value}}</h1>
{{/if}}
{{/each}}
        "#;
        let result = analyze_referenced_partials(&template);
        // TODO: when a partial is used to render something, we should note it
        // TODO: we should list all variables used
        dbg!(result);
        // TODO: Add support for tracing partials used in the template
    }

    #[test]
    fn test_extraction_of_variable_references_where_roles_exist() {
        let template = r#"
---
max_tokens: 64
temperature: 0.7
top_p: 1
---
{{#system}}
Summarize content you are provided with for a second-grade student.
{{/system}}
{{#user}}

{{#each vars}}
  {{content}}
{{/each}}

{{/user}}
        "#;
        let result = analyze_referenced_partials(&template);
        // TODO: when a partial is used to render something, we should note it
        // TODO: we should list all variables used
        let serialized = serde_json::to_string(&result).unwrap();
        dbg!(serialized);
        // TODO: Add support for tracing partials used in the template
    }

    #[test]
    fn test_tracing_partials_used_in_template() {
        let mut partials = HashMap::new();
        partials.insert(
            "part".to_string(),
            PromptLibraryRecord {
                template: "[{{user.name}} inside partial]".to_string(),
                name: "part".to_string(),
                id: "0".to_string(),
                description: None,
            },
        );

        let value = json! {
            {
                "user": {
                    "name": "FirstName",
                    "last_name": "LastName"
                }
            }
        };

        let template = "Basic template {{> part}}";
        let schema = analyze_referenced_partials(&template);
        // TODO: when a partial is used to render something, we should note it
        // TODO: we should list all variables used
        dbg!(schema);
        // TODO: Add support for tracing partials used in the template
    }

    #[test]
    fn test_extracting_role_identifiers() {
        let template_string = indoc! {"
                {{#system}}You are a helpful assistant.{{value}}{{/system}}
                {{#user}}test{{/user}}
                {{#assistant}}test{{/assistant}}
            "};
        let role_blocks = extract_roles_from_template(&template_string);
        assert_eq!(role_blocks[0].0, ChatModelRoles::System);
        assert_eq!(
            role_blocks[0].1.clone().unwrap().source,
            "You are a helpful assistant.{{value}}".to_string()
        );
        assert_eq!(role_blocks[1].0, ChatModelRoles::User);
        assert_eq!(role_blocks[1].1.clone().unwrap().source, "test".to_string());
        assert_eq!(role_blocks[2].0, ChatModelRoles::Assistant);
        assert_eq!(role_blocks[2].1.clone().unwrap().source, "test".to_string());
    }

    #[test]
    fn test_extracting_frontmatter() {
        let template_string = indoc! {"
                ---
                test: 1
                two: 2
                ---
                actual body
            "};
        let result = split_frontmatter(&template_string);
        match result {
            Ok((frontmatter, body)) => {
                assert_eq!(frontmatter, "test: 1\ntwo: 2");
                assert_eq!(body, "actual body");
            }
            Err(_) => {
                assert!(false);
            }
        }
    }

    #[test]
    fn test_constructing_schema() {
        let schema = referenced_variable_list_to_schema(vec![
            ReferencedVariable {
                path: vec![BlockContextElement::Partial("partialName".to_string())],
                is_param: false,
                name: "partialNameNested".to_string(),
            },
            ReferencedVariable {
                path: vec![BlockContextElement::Each("eachVarName".to_string())],
                is_param: true,
                name: "eachVarName".to_string(),
            },
            ReferencedVariable {
                path: vec![BlockContextElement::Each("eachVarName".to_string())],
                is_param: false,
                name: "eachVarReferredInBody".to_string(),
            },
            ReferencedVariable {
                path: vec![BlockContextElement::With("withVarName".to_string())],
                is_param: true,
                name: "withVarName".to_string(),
            },
            ReferencedVariable {
                path: vec![BlockContextElement::With("withVarName".to_string())],
                is_param: false,
                name: "withVarReferredInBody".to_string(),
            },
        ]);
        dbg!(&schema);
    }
}
