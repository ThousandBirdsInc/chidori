use indoc::indoc;
use starlark::environment::{Globals, Module as StarlarkModule};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value as StarlarkValue;

pub fn source_code_run_starlark(source_code: String) -> Option<serde_json::Value> {
    let ast: AstModule = AstModule::parse(
        "hello_world.star",
        source_code.to_owned(),
        &Dialect::Standard,
    )
    .unwrap();
    let globals: Globals = Globals::standard();
    let module: StarlarkModule = StarlarkModule::new();
    let mut eval: Evaluator = Evaluator::new(&module);
    let res: StarlarkValue = eval.eval_module(ast, &globals).unwrap();
    let v: serde_json::Value = serde_json::from_str(&res.to_json().unwrap()).unwrap();
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_code_run_starlark() {
        // Define a sample Starlark code
        let starlark_code = indoc! { r#"
            # Starlark code that outputs JSON data
            def main():
                return {"key": "value"}
                
            main()
        "#}
        .to_string();

        let expected_output = serde_json::json!({"key": "value"});
        let actual_output = source_code_run_starlark(starlark_code).unwrap();
        assert_eq!(actual_output, expected_output);
    }
}
