pub mod language;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use wasm_bindgen::prelude::*;

use rustpython_parser::{ast, Parse};

use crate::language::python::parse::extract_dependencies_python as extract_dependencies_python_impl;

#[wasm_bindgen]
extern "C" {
    // #[wasm_bindgen(js_namespace = console)]
    // fn log(s: &str);
}

#[wasm_bindgen]
pub fn extract_dependencies_python(source_code: &str) -> Result<JsValue, JsValue> {
    let result = extract_dependencies_python_impl(source_code);
    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}
