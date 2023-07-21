use deno_core::serde_json;
use deno_core::serde_json::Value;
use prompt_graph_core::proto2::{ChangeValue, PromptGraphNodeCodeSourceCode};
use prompt_graph_core::templates::render_template_prompt;
use starlark::syntax::{AstModule, Dialect};
use starlark::environment::{Globals, Module as StarlarkModule};
use starlark::eval::Evaluator;
use starlark::values::Value as StarlarkValue;

#[cfg(feature = "starlark")]
pub fn source_code_run_starlark(c: &PromptGraphNodeCodeSourceCode, change_set: &Vec<ChangeValue>) -> Option<Value> {
    let source_code = if c.template {
        render_template_prompt(&c.source_code, &change_set).unwrap()
    } else {
        c.source_code.clone()
    };

    let ast: AstModule = AstModule::parse("hello_world.star", source_code.to_owned(), &Dialect::Standard).unwrap();
    let globals: Globals = Globals::standard();
    let module: StarlarkModule = StarlarkModule::new();
    let mut eval: Evaluator = Evaluator::new(&module);
    let res: StarlarkValue = eval.eval_module(ast, &globals).unwrap();
    let v: Value = serde_json::from_str(&res.to_json().unwrap()).unwrap();
    Some(v)
}



#[cfg(test)]
mod tests {
    use protobuf::EnumOrUnknown;
    use indoc::indoc;
    use prompt_graph_core::proto2::prompt_graph_node_code::Source::SourceCode;
    use prompt_graph_core::proto2::{PromptGraphNodeCode, PromptGraphNodeCodeSourceCode, SupportedSourceCodeLanguages};
    use crate::runtime_nodes::node_code::node::execute_node_code;
    use super::*;

    #[test]
    fn test_exec_code_node_starlark_basic() {
        let mut change_set: Vec<ChangeValue> = vec![];
        let mut output_filled_values: Vec<ChangeValue> = vec![];
        let node = PromptGraphNodeCode {
            name: "".to_string(),
            query: Default::default(),
            output: Default::default(),
            source: Some(SourceCode(PromptGraphNodeCodeSourceCode {
                language: SupportedSourceCodeLanguages::Starlark as i32,
                source_code: indoc! { r#"
                def hello():
                    return "hello"
                { "output": hello() + " world!" }
                "#}.to_string(),
                template: false,
            })),
        };
        execute_node_code(
            &change_set,
            &node);
    }
}
