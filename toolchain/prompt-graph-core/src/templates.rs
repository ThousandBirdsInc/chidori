use std::collections::HashMap;

/// This is a wasm-compatible implementation of how we handle templates for prompts
/// I made the decision to implement this in order to avoid needing to build equivalents for multiple platforms.
use handlebars::{Handlebars, Path, Template};
use handlebars::template::{Parameter, TemplateElement};
use serde_json::{Map, Value};
use serde_json::value::{Map as JsonMap};


use anyhow::{Result};
use crate::proto2::serialized_value::Val;
use crate::proto2::{ChangeValue, PromptLibraryRecord, SerializedValue, SerializedValueArray, SerializedValueObject};


// https://github.com/microsoft/guidance

// TODO: support accessing a library of prompts injected as partials

// https://github.com/sunng87/handlebars-rust/blob/23ca8d76bee783bf72f627b4c4995d1d11008d17/src/template.rs#L963
// self.handlebars.register_template_string(name, template).unwrap();

pub fn validate_template(template_str: &str, _query_paths: Vec<Vec<String>>) {
    // let mut handlebars = Handlebars::new();
    let template = Template::compile(template_str).unwrap();
    let mut reference_paths = Vec::new();
    traverse_ast(&template, &mut reference_paths, vec![]);
    println!("{:?}", reference_paths);
    // TODO: check that all query paths are satisfied by this template
    // handlebars.register_template("test", template).unwrap();
}

#[derive(Debug, Clone)]
struct ContextBlock {
    name: Parameter,
    params: Vec<Parameter>,
}

fn traverse_ast(template: &Template, reference_paths: &mut Vec<(Path, Vec<ContextBlock>)>, context: Vec<ContextBlock>) {
    for el in &template.elements {
        match el {
            TemplateElement::RawString(_) => {}
            TemplateElement::HtmlExpression(helper_block) |
            TemplateElement::Expression(helper_block) |
            TemplateElement::HelperBlock(helper_block) => {
                let deref = *(helper_block.clone());
                let _params = &deref.params;
                match &deref.name {
                    Parameter::Name(_name) => {
                        // println!("name, {:?} - params {:?}", name, params);
                        // reference_paths.push((None, context.clone()));
                    }
                    Parameter::Path(path) => {
                        reference_paths.push((path.clone(), context.clone()));
                    }
                    Parameter::Literal(_) => {
                    }
                    Parameter::Subexpression(_) => {}
                }
                if let Some(next_template) = deref.template {
                    let mut ctx = context.clone();
                    ctx.extend(vec![ContextBlock {
                        name: deref.name.clone(),
                        params: deref.params.clone(),
                    }]);
                    traverse_ast(&next_template, reference_paths, ctx);
                }
            }
            TemplateElement::DecoratorExpression(_) => {}
            TemplateElement::DecoratorBlock(_) => {}
            TemplateElement::PartialExpression(_) => {}
            TemplateElement::PartialBlock(_) => {}
            TemplateElement::Comment(_) => {}
        }
    }
}

fn convert_template_to_prompt() {

}

fn infer_query_from_template() {

}

fn extract_roles_from_template() {

}

pub fn flatten_value_keys(sval: SerializedValue, current_path: Vec<String>) -> Vec<(Vec<String>, Val)> {
    let mut flattened = vec![];
    match sval.val {
        Some(Val::Object(a)) => {
            for (key, value) in &a.values {
                let mut path = current_path.clone();
                path.push(key.clone());
                flattened.extend(flatten_value_keys(value.clone(), path));
            }
        }
        None => {},
        x @ _ => { flattened.push((current_path.clone(), x.unwrap())) }
    }
    flattened
}

// TODO: fix the conversion to numbers
pub fn serialized_value_to_json_value(sval: &SerializedValue) -> Value {
    match &sval.val {
        Some(Val::Float(f)) => { Value::Number(f.to_string().parse().unwrap()) }
        Some(Val::Number(n)) => { Value::Number(n.to_string().parse().unwrap()) }
        Some(Val::String(s)) => { Value::String(s.to_string()) }
        Some(Val::Boolean(b)) => { Value::Bool(*b) }
        Some(Val::Array(a)) => {
            Value::Array(a.values.iter().map(|v| serialized_value_to_json_value(v)).collect())
        }
        Some(Val::Object(a)) => {
            Value::Object(a.values.iter().map(|(k, v)| (k.clone(), serialized_value_to_json_value(v))).collect())
        }
        _ => { Value::Null }
    }
}

pub fn json_value_to_serialized_value(jval: &Value) -> SerializedValue {
    SerializedValue {
        val: match jval {
            Value::Number(n) => {
                if n.is_i64() {
                    Some(Val::Number(n.as_i64().unwrap() as i32))
                } else if n.is_f64() {
                    Some(Val::Float(n.as_f64().unwrap() as f32))
                } else {
                    panic!("Invalid number value")
                }
            }
            Value::String(s) => Some(Val::String(s.clone())),
            Value::Bool(b) => Some(Val::Boolean(*b)),
            Value::Array(a) => {
                Some(Val::Array(SerializedValueArray{ values: a.iter().map(|v| json_value_to_serialized_value(v)).collect()}))
            }
            Value::Object(o) => {
                let mut map = HashMap::new();
                for (k, v) in o {
                    map.insert(k.clone(), json_value_to_serialized_value(v));
                }
                Some(Val::Object(SerializedValueObject{ values: map }))
            }
            Value::Null => None,
            _ => panic!("Invalid value type"),
        },
    }
}




fn query_path_to_json(path: &[String], val: &SerializedValue) -> Option<Map<String, Value>> {
    let mut map = JsonMap::new();
    if let Some((head, tail)) = path.split_first() {
        if tail.is_empty() {
            map.insert(head.clone(), serialized_value_to_json_value(val));
        } else {
            if let Some(created) = query_path_to_json(tail, val) {
                map.insert(head.clone(), Value::Object(created));
            }
        }
        Some(map)
    } else {
        None
    }
}

fn merge(a: &mut Value, b: Value) {
    if let Value::Object(a) = a {
        if let Value::Object(b) = b {
            for (k, v) in b {
                if v.is_null() {
                    a.remove(&k);
                }
                else {
                    merge(a.entry(k).or_insert(Value::Null), v);
                }
            }

            return;
        }
    }

    *a = b;
}

fn query_paths_to_json(query_paths: &Vec<ChangeValue>) -> Value {
    let mut m = Value::Object(JsonMap::new());
    for change_value in query_paths {
        let path = &change_value.path.as_ref().unwrap().address;
        let val = &change_value.value.as_ref().unwrap();
        if let Some(created) = query_path_to_json(path, val) {
            merge(&mut m, Value::Object(created));
        }
    }
    m
}

// TODO: remove these unwraps
// TODO: add an argument for passing a set of partials
// TODO: implement block helpers for User and System prompts



pub fn render_template_prompt(template_str: &str, query_paths: &Vec<ChangeValue>, partials: &HashMap<String, PromptLibraryRecord>) -> Result<String> {
    let mut reg = Handlebars::new();
    for (name, prompt) in partials.iter() {
        reg.register_partial(name, prompt.record.as_ref().unwrap().template.as_str()).unwrap();
    }
    reg.register_template_string("tpl_1", template_str).unwrap();
    reg.register_escape_fn(handlebars::no_escape);
    let render = reg.render("tpl_1", &query_paths_to_json(query_paths)).unwrap();
    Ok(render)
}


#[cfg(test)]
mod tests {
    use serde_json::json;
    use crate::create_change_value;
    use crate::proto2::UpsertPromptLibraryRecord;
    use super::*;
    use crate::templates::validate_template;

    #[test]
    fn test_generating_json_map_from_paths() {
        assert_eq!(query_paths_to_json(&vec![
            create_change_value(
                vec![String::from("user"), String::from("name")],
                Some(Val::String(String::from("John"))),
                0
            ),
        ]), json!({
            "user": {
                "name": "John",
            }})
        );

        assert_eq!(query_paths_to_json(&vec![
            create_change_value(
                vec![String::from("user"), String::from("name")],
                Some(Val::String(String::from("John"))),
                0
            ),
            create_change_value(
                vec![String::from("user"), String::from("last_name")],
                Some(Val::String(String::from("Panhuyzen"))),
                0
            )
        ]), json!({
            "user": {
                "name": "John",
                "last_name": "Panhuyzen"
            }})
        );
    }

    #[test]
    fn test_template_validation() {
        validate_template(
            "Hello, {{name}}! {{user.name}}",
            vec![vec!["user".to_string(), "name".to_string()]],
        );
    }

    #[test]
    fn test_template_validation_eval_context() {
        validate_template(
            "{{#with user}} {{name}} {{/with}}",
            vec![vec!["user".to_string(), "name".to_string()]],
        );
    }

    #[test]
    fn test_template_validation_eval_context_each() {
        validate_template(
            "{{#each users}} {{name}} {{/each}}",
            vec![vec!["user".to_string(), "name".to_string()]],
        );
    }

    #[test]
    fn test_guidance_style_system_prompts() {
        validate_template(
            "\
                {{#system}}
                You are a helpful assistant. {{value}}
                {{/system}}
                {{#user}}
                    test
                {{/user}}
                {{#assistant}}
                    test
                {{/assistant}}
            ",
            vec![vec!["user".to_string(), "name".to_string()]],
        );
    }

    #[test]
    fn test_rendering_template() {
        let rendered = render_template_prompt(
            &"Basic template {{user.name}}",
            &vec![
                create_change_value(
                    vec![String::from("user"), String::from("name")],
                    Some(Val::String(String::from("John"))),
                    0
                ),
                create_change_value(
                    vec![String::from("user"), String::from("last_name")],
                    Some(Val::String(String::from("Panhuyzen"))),
                    0
                )
            ],
            &HashMap::new()
        );
        assert_eq!(rendered.unwrap(), "Basic template John");
    }

    #[test]
    fn test_rendering_template_with_partial() {
        let mut partials = HashMap::new();
        partials.insert("part".to_string(), PromptLibraryRecord {
            record: Some(UpsertPromptLibraryRecord {
                template: "[{{user.name}} inside partial]".to_string(),
                name: "part".to_string(),
                id: "0".to_string(),
                description: None,
            }),
            version_counter: 0,
        });

        let rendered = render_template_prompt(
            &"Basic template {{> part}}",
            &vec![
                create_change_value(
                    vec![String::from("user"), String::from("name")],
                    Some(Val::String(String::from("John"))),
                    0
                ),
                create_change_value(
                    vec![String::from("user"), String::from("last_name")],
                    Some(Val::String(String::from("Panhuyzen"))),
                    0
                )
            ],
            &partials
        );
        assert_eq!(rendered.unwrap(), "Basic template [John inside partial]");
    }


}
