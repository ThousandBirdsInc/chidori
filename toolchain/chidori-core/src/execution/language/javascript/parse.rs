extern crate swc_ecma_parser;
use swc_common::sync::Lrc;
use swc_common::{
    errors::{ColorConfig, Handler},
    FileName, FilePathMapping, SourceMap,
};
use swc_ecma_ast::{Decl, FnDecl, ModuleItem, Stmt};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluation_single_node() {
        let cm: Lrc<SourceMap> = Default::default();
        let handler = Handler::with_tty_emitter(ColorConfig::Auto, true, false, Some(cm.clone()));

        // Real usage
        // let fm = cm
        //     .load_file(Path::new("test.js"))
        //     .expect("failed to load test.js");
        let fm = cm.new_source_file(
            FileName::Custom("test.js".into()),
            r#"
            function bar() {
               console.log("bar");
            }
            
            function foo() {
                let test = prompt.split("\n");
                console.log("Hello World!");
            }"#
            .into(),
        );
        let lexer = Lexer::new(
            // We want to parse ecmascript
            Syntax::Es(Default::default()),
            // EsVersion defaults to es5
            Default::default(),
            StringInput::from(&*fm),
            None,
        );

        let mut parser = Parser::new_from(lexer);

        for e in parser.take_errors() {
            e.into_diagnostic(&handler).emit();
        }

        let module = parser
            .parse_module()
            .map_err(|mut e| {
                // Unrecoverable fatal error occurred
                e.into_diagnostic(&handler).emit()
            })
            .expect("failed to parser module");

        for item in module.body {
            if let ModuleItem::Stmt(stmt) = item {
                match stmt {
                    Stmt::Decl(Decl::Fn(FnDecl {
                        ident, function, ..
                    })) => {
                        println!("Function name: {}", ident.sym);
                        println!("Function body: {:?}", function.body);
                    }
                    _ => {}
                }
            }
        }
    }
}
