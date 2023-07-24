use apollo_parser::ast::Document;
use apollo_parser::ast;
use crate::proto2::serialized_value::Val;
use crate::proto2::SerializedValue;


pub mod wasm_error;

pub fn uppercase_first_letter(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub fn lowercase_first_letter(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
    }
}

fn print_graphql_type_def(doc: Document) {
    for def in doc.definitions() {
        if let ast::Definition::ObjectTypeDefinition(object_type) = def {
            println!("{:?}", object_type.name().unwrap().text());
            for field_def in object_type.fields_definition().unwrap().field_definitions() {
                println!("{}", field_def.name().unwrap().text()); // size weight
            }
        }
    }
}

pub fn serialized_value_to_string(v: &SerializedValue) -> String {
    if let Some(v) = &v.val {
        match v {
            Val::Float(f) => { f.to_string() }
            Val::Number(f) => { f.to_string() }
            Val::String(s) => { s.to_string()}
            Val::Boolean(b) => { b.to_string()}
            Val::Array(a) => {
                a.values.iter()
                    .map(|v| serialized_value_to_string(&v.clone()))
                    .collect::<Vec<String>>().join(", ")
            }
            Val::Object(o) => {
                o.values.iter()
                    .map(|(k, v)| format!("{}: {}", k, serialized_value_to_string(&v.clone())))
                    .collect::<Vec<String>>().join(", ")
            }
        }
    } else {
        String::from("None")
    }

}
