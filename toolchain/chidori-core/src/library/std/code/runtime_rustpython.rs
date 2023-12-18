use rustpython_vm as vm;

fn source_code_run_python(source_code: String) -> vm::PyResult<()> {
    vm::Interpreter::without_stdlib(Default::default()).enter(|vm| {
        let scope = vm.new_scope_with_builtins();
        let code_obj = vm
            .compile(
                &source_code,
                vm::compiler::Mode::Exec,
                "<embedded>".to_owned(),
            )
            .map_err(|err| vm.new_syntax_error(&err, Some(&source_code)))?;

        vm.run_code_obj(code_obj, scope)?;

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_code_run_py_success() {
        let source_code = String::from("return 42;");
        let result = source_code_run_python(source_code);
        // assert_eq!(result.unwrap(), Some(serde_json::json!(42)));
    }
}
