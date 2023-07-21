use apollo_parser::ast::Document;
use apollo_parser::ast;


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
