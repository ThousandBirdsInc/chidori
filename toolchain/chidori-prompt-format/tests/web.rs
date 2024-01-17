//! Test suite for the Web and headless browsers.

#![cfg(target_arch = "wasm32")]

extern crate wasm_bindgen_test;

use chidori_prompt_format::analyze_referenced_partials;
use chidori_prompt_format::templating::templates::{SchemaItem, SchemaItemType};
use std::collections::HashMap;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn analyzing_referenced_partials() {
    let template = "Basic template {{var}} {{dot.notation}}";
    let result = analyze_referenced_partials(&template);
    let val: SchemaItem = serde_wasm_bindgen::from_value(result).unwrap();
    assert_eq!(
        val,
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
}
