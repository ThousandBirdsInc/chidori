use rustpython_parser::{ast, Parse};

fn example() {
    let python_source = r#"
def is_odd(i):
  return bool(i & 1)
"#;
    let ast = ast::Suite::parse(python_source, "<embedded>");

    assert!(ast.is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_evaluation_single_node() {
        let python_source = indoc! { r#"
            def bar():
               print("bar")
            
            def foo():
                test = prompt.split("\n")
                print("Hello World!")
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();

        for item in ast {
            match item {
                ast::Stmt::FunctionDef(ast::StmtFunctionDef { name, .. }) => {
                    dbg!(name);
                }
                _ => {}
            }
        }
    }
}
