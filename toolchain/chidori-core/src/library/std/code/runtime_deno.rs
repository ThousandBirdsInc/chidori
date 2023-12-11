use anyhow::Result;
use deno_core::serde_json::Value;
use deno_core::{serde_json, serde_v8, v8, FastString, JsRuntime, RuntimeOptions};

pub fn source_code_run_deno(source_code: String, _state: Option<Value>) -> Result<Option<Value>> {
    // Wrap the source code in an entrypoint function so that it immediately evaluates
    let wrapped_source_code = format!(
        r#"(function main() {{
        {}
    }})();"#,
        source_code
    );

    let mut runtime = JsRuntime::new(RuntimeOptions::default());

    // TODO: the script receives the arguments as a json payload "#state"
    let result = runtime.execute_script(
        "main.js",
        FastString::Owned(wrapped_source_code.into_boxed_str()),
    );

    match result {
        Ok(global) => {
            let scope = &mut runtime.handle_scope();
            let local = v8::Local::new(scope, global);
            let deserialized_value = serde_v8::from_v8::<serde_json::Value>(scope, local);
            return Ok(if let Ok(value) = deserialized_value {
                Some(value)
            } else {
                None
            });
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_code_run_deno_success() {
        let source_code = String::from("return 42;");
        let result = source_code_run_deno(source_code, None);
        assert_eq!(result.unwrap(), Some(serde_json::json!(42)));
    }

    #[test]
    fn test_source_code_run_deno_failure() {
        let source_code = String::from("throw new Error('Test Error');");
        let result = source_code_run_deno(source_code, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_source_code_run_deno_json_serialization() {
        let source_code = String::from("return {foo: 'bar'};");
        let result = source_code_run_deno(source_code, None);
        assert_eq!(result.unwrap(), Some(serde_json::json!({"foo": "bar"})));
    }
}
