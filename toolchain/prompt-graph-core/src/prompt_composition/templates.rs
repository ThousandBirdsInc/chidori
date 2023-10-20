use crate::execution::primitives::serialized_value::RkyvSerializedValue as RKV;
///! The goal of prompt_composition is to enable the composition of prompts in a way where we can
///! trace how the final prompt was assembled and why.
///!
///! This is a wasm-compatible implementation of how we handle templates for prompts
use anyhow::Result;
use handlebars::template::{Parameter, TemplateElement};
use handlebars::{Handlebars, Path, Template};
use serde_json::value::Map as JsonMap;
use serde_json::{Map, Value};
use std::collections::HashMap;

// https://github.com/microsoft/guidance

// TODO: support accessing a library of prompts injected as partials

// https://github.com/sunng87/handlebars-rust/blob/23ca8d76bee783bf72f627b4c4995d1d11008d17/src/template.rs#L963
// self.handlebars.register_template_string(name, template).unwrap();

// /// Verify that the template and included query paths are valid
// pub fn validate_template(template_str: &str, _query_paths: Vec<Vec<String>>) {
//     // let mut handlebars = Handlebars::new();
//     let template = Template::compile(template_str).unwrap();
//     let mut reference_paths = Vec::new();
//     println!("{:?}", reference_paths);
//     // TODO: check that all query paths are satisfied by this template
//     // handlebars.register_template("test", template).unwrap();
// }

#[derive(Debug, Clone)]
struct ContextBlock {
    name: Parameter,
    params: Vec<Parameter>,
}

/// Traverse over every partial template in a Template (which can be a set of template partials) and validate that each
/// partial template can be matched to a either 1) some template type that Handlebars recognizes
/// or 2) a query path that can pull data out of the event log
fn analyze_referenced_partials(
    template: &Template,
    reference_paths: &mut Vec<(Path, Vec<ContextBlock>)>,
    context: Vec<ContextBlock>,
    partial_library: &HashMap<String, PromptLibraryRecord>,
) {
    for el in &template.elements {
        match el {
            TemplateElement::RawString(_) => {}
            TemplateElement::HtmlExpression(helper_block)
            | TemplateElement::Expression(helper_block)
            | TemplateElement::HelperBlock(helper_block) => {
                let deref = *(helper_block.clone());
                if let Some(next_template) = deref.template {
                    let mut ctx = context.clone();
                    ctx.extend(vec![ContextBlock {
                        name: deref.name.clone(),
                        params: deref.params.clone(),
                    }]);
                    analyze_referenced_partials(
                        &next_template,
                        reference_paths,
                        ctx,
                        partial_library,
                    );
                }
            }
            TemplateElement::DecoratorExpression(decorator_block) => {}
            TemplateElement::DecoratorBlock(_) => {}
            TemplateElement::PartialExpression(x) => {
                println!("PartialExpression {:?}", x);
                let deref = *(x.clone());
                if let Some(ident) = deref.indent {
                    if let Some(record) = partial_library.get(&ident) {
                        let mut ctx = context.clone();
                        ctx.extend(vec![ContextBlock {
                            name: deref.name.clone(),
                            params: deref.params.clone(),
                        }]);
                        // TODO: recursively analyze the partial template
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

fn convert_template_to_prompt() {}

fn infer_query_from_template() {}

#[derive(PartialEq, Debug)]
enum ChatModelRoles {
    User,
    System,
    Assistant,
}

fn extract_roles_from_template(
    template: &Template,
    context: Vec<ContextBlock>,
) -> Vec<(ChatModelRoles, Option<Template>)> {
    let mut role_blocks: Vec<(ChatModelRoles, Option<Template>)> = vec![];
    for el in &template.elements {
        match el {
            TemplateElement::RawString(_) => {}
            TemplateElement::HtmlExpression(helper_block)
            | TemplateElement::Expression(helper_block)
            | TemplateElement::HelperBlock(helper_block) => {
                let deref = *(helper_block.clone());
                let params = &deref.params;
                match &deref.name {
                    Parameter::Name(name) => {
                        match name.as_str() {
                            "user" => {
                                role_blocks.push((ChatModelRoles::User, deref.template.clone()));
                            }
                            "system" => {
                                role_blocks.push((ChatModelRoles::System, deref.template.clone()));
                            }
                            "assistant" => {
                                role_blocks
                                    .push((ChatModelRoles::Assistant, deref.template.clone()));
                            }
                            _ => {}
                        }
                        println!("Parameter::Name name, {:?} - params {:?}", name, params,);
                        println!("Template {:?}", deref.template);
                    }
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
                    let roles = extract_roles_from_template(&next_template, ctx);
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

// TODO: fix the conversion to numbers
/// Convert a SerializedValue into a serde_json::Value
pub fn serialized_value_to_json_value(v: &RKV) -> Value {
    match &v {
        RKV::Float(f) => Value::Number(f.to_string().parse().unwrap()),
        RKV::Number(n) => Value::Number(n.to_string().parse().unwrap()),
        RKV::String(s) => Value::String(s.to_string()),
        RKV::Boolean(b) => Value::Bool(*b),
        RKV::Array(a) => Value::Array(
            a.iter()
                .map(|v| serialized_value_to_json_value(v))
                .collect(),
        ),
        RKV::Object(a) => Value::Object(
            a.iter()
                .map(|(k, v)| (k.clone(), serialized_value_to_json_value(v)))
                .collect(),
        ),
        RKV::FunctionPointer(_) => Value::Null,
        RKV::StreamPointer(_) => Value::Null,
        RKV::Null => Value::Null,
    }
}

/// Convert a serde_json::Value into a SerializedValue
pub fn json_value_to_serialized_value(jval: &Value) -> RKV {
    match jval {
        Value::Number(n) => {
            if n.is_i64() {
                RKV::Number(n.as_i64().unwrap() as i32)
            } else if n.is_f64() {
                RKV::Float(n.as_f64().unwrap() as f32)
            } else {
                panic!("Invalid number value")
            }
        }
        Value::String(s) => RKV::String(s.clone()),
        Value::Bool(b) => RKV::Boolean(*b),
        Value::Array(a) => RKV::Array(
            a.iter()
                .map(|v| json_value_to_serialized_value(v))
                .collect(),
        ),
        Value::Object(o) => {
            let mut map = HashMap::new();
            for (k, v) in o {
                map.insert(k.clone(), json_value_to_serialized_value(v));
            }
            RKV::Object(map)
        }
        Value::Null => RKV::Null,
        _ => panic!("Invalid value type"),
    }
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
    values: &RKV,
    partials: &HashMap<String, PromptLibraryRecord>,
) -> Result<String> {
    let json_value = serialized_value_to_json_value(values);
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

#[cfg(test)]
mod tests {
    use super::*;
    use gluesql::core::ast_builder::extract;
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
        let mut map = HashMap::new();
        map.insert("name".to_string(), RKV::String("FirstName".to_string()));
        map.insert("last_name".to_string(), RKV::String("LastName".to_string()));
        let value = RKV::Object(map);
        let mut map = HashMap::new();
        map.insert("user".to_string(), value);
        let value = RKV::Object(map);

        let rendered =
            render_template_prompt(&"Basic template {{user.name}}", &value, &HashMap::new());
        assert_eq!(rendered.unwrap(), "Basic template FirstName");
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

        let mut map = HashMap::new();
        map.insert("name".to_string(), RKV::String("FirstName".to_string()));
        map.insert("last_name".to_string(), RKV::String("LastName".to_string()));
        let value = RKV::Object(map);
        let mut map = HashMap::new();
        map.insert("user".to_string(), value);
        let value = RKV::Object(map);

        let rendered = render_template_prompt(&"Basic template {{> part}}", &value, &partials);
        assert_eq!(
            rendered.unwrap(),
            "Basic template [FirstName inside partial]"
        );
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

        let mut map = HashMap::new();
        map.insert("name".to_string(), RKV::String("FirstName".to_string()));
        map.insert("last_name".to_string(), RKV::String("LastName".to_string()));
        let value = RKV::Object(map);
        let mut map = HashMap::new();
        map.insert("user".to_string(), value);
        let value = RKV::Object(map);

        let template = Template::compile("Basic template {{> part}}").unwrap();
        let mut paths = vec![];
        analyze_referenced_partials(&template, &mut paths, vec![], &partials);
        // TODO: Add support for tracing partials used in the template
    }

    #[test]
    fn test_extracting_role_identifiers() {
        let template = Template::compile(indoc! {"
                {{#system}}You are a helpful assistant.{{value}}{{/system}}
                {{#user}}test{{/user}}
                {{#assistant}}test{{/assistant}}
            "})
        .unwrap();
        let role_blocks = extract_roles_from_template(&template, vec![]);
        assert_eq!(role_blocks[0].0, ChatModelRoles::System);
        assert_eq!(
            role_blocks[0].1.clone().unwrap().elements[0],
            Template::compile("You are a helpful assistant. {{value}}")
                .unwrap()
                .elements[0]
        );
        assert_eq!(role_blocks[1].0, ChatModelRoles::User);
        assert_eq!(
            role_blocks[1].1.clone().unwrap().elements[0],
            Template::compile("test").unwrap().elements[0]
        );
        assert_eq!(role_blocks[2].0, ChatModelRoles::Assistant);
        assert_eq!(
            role_blocks[2].1.clone().unwrap().elements[0],
            Template::compile("test").unwrap().elements[0]
        );
    }
}
