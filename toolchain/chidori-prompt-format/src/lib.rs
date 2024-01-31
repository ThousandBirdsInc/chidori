pub mod templating;
mod utils;
use handlebars::Template;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use wasm_bindgen::prelude::*;

use crate::templating::templates::{ChatModelRoles, PromptLibraryRecord, TemplateWithSource};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    // #[wasm_bindgen(js_namespace = console)]
    // fn log(s: &str);
}

#[wasm_bindgen]
pub fn render_template_prompt(
    template_str: &str,
    json_value: JsValue,
    // partials_json: JsValue,
) -> Result<JsValue, JsValue> {
    let json_value: Value = serde_wasm_bindgen::from_value(json_value)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    // let partials: HashMap<String, PromptLibraryRecord> =
    //     serde_wasm_bindgen::from_value(partials_json)
    //         .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let partials: HashMap<String, PromptLibraryRecord> = HashMap::new();

    let result =
        crate::templating::templates::render_template_prompt(template_str, &json_value, &partials)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[derive(Serialize, Deserialize, Debug)]
struct TemplateWithRole {
    role: ChatModelRoles,
    source: String,
}

#[wasm_bindgen]
pub fn extract_roles_from_template(template: &str) -> JsValue {
    let mut role_blocks = crate::templating::templates::extract_roles_from_template(&template);
    let templates_with_roles: Vec<TemplateWithRole> = role_blocks
        .into_iter()
        .map(|(a, b)| TemplateWithRole {
            role: a,
            source: b.unwrap().source.to_string(),
        })
        .collect();
    serde_wasm_bindgen::to_value(&templates_with_roles)
        .map_err(|e| JsValue::from_str(&e.to_string()))
        .unwrap()
}

#[wasm_bindgen]
pub fn extract_yaml_frontmatter(template: &str) -> JsValue {
    let result = crate::templating::templates::extract_frontmatter(&template).unwrap();
    let deserialized_data: HashMap<String, String> = serde_yaml::from_str(&result.0).unwrap();
    serde_wasm_bindgen::to_value(&(deserialized_data, result.1))
        .map_err(|e| JsValue::from_str(&e.to_string()))
        .unwrap()
}

#[wasm_bindgen]
pub fn analyze_referenced_partials(template: &str) -> JsValue {
    let schema = crate::templating::templates::analyze_referenced_partials(&template);
    serde_wasm_bindgen::to_value(&schema)
        .map_err(|e| JsValue::from_str(&e.to_string()))
        .unwrap()
}
