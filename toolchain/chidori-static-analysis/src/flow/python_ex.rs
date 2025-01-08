use ruff_python_ast::{Expr, ExprCall, ExprAttribute, ExprName, Stmt, StmtExpr, ModModule, Suite, ExprContext, ConversionFlag};
use crate::ruff_python_codegen::{Generator, Stylist};
use ruff_python_parser::ParseError;

fn insert_logging(module: &mut ModModule) {
    // Walk through all statements and find function definitions
    for stmt in &mut module.body {
        if let Stmt::FunctionDef(func_def) = stmt {
            // Create a logging statement: print(f"Entering function {func_name}")
            let log_stmt = Stmt::Expr(StmtExpr {
                range: Default::default(),
                value: Box::new(Expr::Call(ExprCall {
                    range: Default::default(),
                    func: Box::new(Expr::Name(ExprName {
                        range: Default::default(),
                        id: "print".into(),
                        ctx: ExprContext::Load,
                    })),
                    arguments: ruff_python_ast::Arguments {
                        range: Default::default(),
                        args: vec![Expr::FString(ruff_python_ast::ExprFString {
                            range: Default::default(),
                            value: ruff_python_ast::FStringValue::single(ruff_python_ast::FString {
                                range: Default::default(),
                                elements: vec![
                                    ruff_python_ast::FStringElement::Literal(ruff_python_ast::FStringLiteralElement {
                                        range: Default::default(),
                                        value: "Entering function ".into(),
                                    }),
                                    ruff_python_ast::FStringElement::Literal(
                                        ruff_python_ast::FStringLiteralElement {
                                            range: Default::default(),
                                            value: func_def.name.id.as_str().into(),
                                        }
                                    )
                                ].into(),
                                flags: Default::default(),
                            })
                        })].into_boxed_slice(),
                        keywords: vec![].into_boxed_slice(),

                    },
                })),
            });

            // Insert logging statement at the beginning of function body
            func_def.body.insert(0, log_stmt);
        }
    }
}

pub fn transform_code(code: &str) -> Result<String, ParseError> {
    // Parse the input code
    let parse = ruff_python_parser::parse_module(code)?;
    let stylist = Stylist::from_tokens(parse.tokens(), code);
    let mut parsed = parse.into_syntax();

    // Modify the AST
    insert_logging(&mut parsed);


    // Generate code from modified AST
    // let stylist = Stylist::default();
    let mut generator: Generator = (&stylist).into();
    let suite = Suite::from(parsed.body);
    generator.unparse_suite(&suite);
    Ok(generator.generate())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use ruff_source_file::LineEnding;

    #[test]
    fn test_insert_logging() {
        let input = indoc! {r#"
            def greet(name):
                return f"Hello, {name}!"

            def calculate_sum(a, b):
                return a + b
        "#};

        let expected = indoc! {r#"
            def greet(name):
                print(f"Entering function greet")
                return f"Hello, {name}!"


            def calculate_sum(a, b):
                print(f"Entering function calculate_sum")
                return a + b"#}.replace('\n', LineEnding::default().as_str());

        let result = transform_code(input).unwrap();
        assert_eq!(result, expected);
    }
}