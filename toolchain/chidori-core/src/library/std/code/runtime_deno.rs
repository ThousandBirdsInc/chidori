use std::cell::RefCell;
use anyhow::Result;
use deno_core::error::AnyError;
use deno_core::{Extension, ExtensionFileSource, ExtensionFileSourceCode, FastString, JsRuntime, ModuleSpecifier, Op, op2, OpState, PollEventLoopOptions, RuntimeOptions, serde_json, serde_v8, v8};
use deno;
use std::sync::{Arc, Mutex};

use crate::execution::primitives::serialized_value::{
    json_value_to_serialized_value, RkyvObjectBuilder, RkyvSerializedValue,
};
use chidori_static_analysis::language::javascript::parse::{build_report, extract_dependencies_js};
use deno_core::_ops::{RustToV8, RustToV8NoScope};
use deno_core::v8::{Global, Handle, HandleScope};
use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::hash::Hash;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use deno::factory::CliFactory;
use deno::file_fetcher::File;
use deno_runtime::permissions::{Permissions, PermissionsContainer};
use futures_util::FutureExt;
use pyo3::{Py, PyAny};
use pyo3::types::{IntoPyDict, PyTuple};
use tokio::runtime::Runtime;
use crate::cells::{CellTypes, CodeCell, LLMPromptCell};
use crate::execution::execution::ExecutionState;

// TODO: need to override console.log


fn serde_v8_to_rkyv(
    mut scope: &mut HandleScope,
    arg0: v8::Local<v8::Value>,
) -> Result<RkyvSerializedValue, String> {
    let arg0 = match deno_core::_ops::serde_v8_to_rust(&mut scope, arg0) {
        Ok(t) => t,
        Err(arg0_err) => {
            let msg = deno_core::v8::String::new(&mut scope, &{
                let res = std::fmt::format(std::format_args!(
                    "{}",
                    deno_core::anyhow::Error::from(arg0_err.clone())
                ));
                res
            })
            .unwrap();
            let exc = deno_core::v8::Exception::type_error(&mut scope, msg);
            scope.throw_exception(exc);
            return Err(arg0_err.to_string());
        }
    };
    Ok(arg0)
}

struct MyOpState {
    output: Option<RkyvSerializedValue>,
    payload: RkyvSerializedValue,
    cell_depended_values: HashMap<String, String>,
    execution_state_handle: Arc<Mutex<ExecutionState>>,
    functions: HashMap<
        String,
        InternalClosureFnMut,
    >,
}

#[op2]
#[serde]
fn op_assert_eq(
    #[serde] a: RkyvSerializedValue,
    #[serde] b: RkyvSerializedValue,
) -> Result<RkyvSerializedValue, AnyError> {
    if a == b {
        Ok(RkyvSerializedValue::String("Success".to_string()))
    } else {
        println!("Assertion failed @ {:?} != {:?}", a, b);
        Ok(RkyvSerializedValue::String("Failure".to_string()))
    }
}


#[op2(async)]
#[serde]
async fn op_call_rust(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] args: Vec<RkyvSerializedValue>,
    #[serde] kwargs: HashMap<String, RkyvSerializedValue>,
) -> Result<RkyvSerializedValue, AnyError> {
    let mut op_state = state.borrow_mut();
    let mut my_op_state: &mut Arc<Mutex<MyOpState>> = (*op_state).borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    let func = my_op_state.functions.get_mut(&name).unwrap();
    let kwargs = if kwargs.is_empty() {
        None
    } else {
        Some(kwargs)
    };
    let result = func(args, kwargs).await?;
    Ok(result)
}


#[op2]
#[serde]
fn op_save_result<'scope>(
    mut state: &mut OpState,
    #[serde] val: RkyvSerializedValue,
) -> Result<(), AnyError> {
    let mut my_op_state: &Arc<Mutex<MyOpState>> = state.borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    my_op_state.output = Some(val);
    Ok(())
}

#[op2]
#[serde]
fn op_save_result_object<'scope>(
    mut state: &mut OpState,
    #[serde] kwargs: HashMap<String, RkyvSerializedValue>,
) -> Result<(), AnyError> {
    let mut my_op_state: &Arc<Mutex<MyOpState>> = state.borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    let mut output = RkyvObjectBuilder::new();
    for (key, value) in kwargs {
        output = output.insert_value(&key, value);
    }
    // TODO: union with the existing value if there is one
    my_op_state.output = Some(output.build());
    Ok(())
}


#[op2]
#[serde]
fn op_invoke_function<'scope>(
    scope: &mut v8::HandleScope<'scope>,
    mut state: &mut OpState,
    input: v8::Local<v8::Function>,
) -> Result<RkyvSerializedValue, AnyError> {
    let global = scope.get_current_context().global(scope);

    // TODO: handle async functions
    // https://docs.rs/rusty_v8/latest/rusty_v8/struct.PromiseResolver.html

    // Prepare the arguments for the function call, if any.
    let mut args: Vec<_> = vec![];
    let mut kwargs = vec![];

    let mut my_op_state: &Arc<Mutex<MyOpState>> = state.borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    if let RkyvSerializedValue::Object(ref payload_map) = my_op_state.payload {
        if let Some(RkyvSerializedValue::Object(args_map)) = payload_map.get("args")
        {
            let mut args_vec: Vec<_> = args_map
                .iter()
                .map(|(k, v)| (k.parse::<i32>().unwrap(), v))
                .collect();

            args_vec.sort_by_key(|k| k.0);
            args.extend(
                args_vec
                    .into_iter()
                    .map(|(_, v)|
                        deno_core::_ops::RustToV8Fallible::to_v8_fallible(
                            deno_core::_ops::RustToV8Marker::<deno_core::_ops::SerdeMarker, _>::from(
                                v,
                            ),
                            scope,
                        ).unwrap()
                    ),
            );
        }

        if let Some(RkyvSerializedValue::Object(kwargs_map)) =
            payload_map.get("kwargs")
        {
            for (k, v) in kwargs_map.iter() {
                kwargs.push((k,
                             deno_core::_ops::RustToV8Fallible::to_v8_fallible(
                                 deno_core::_ops::RustToV8Marker::<deno_core::_ops::SerdeMarker, _>::from(
                                     v,
                                 ),
                                 scope,
                             ).unwrap()
                ));
            }
        }
    }

    // Invoke the JavaScript function. The result is wrapped in a Result type.
    let result = input.call(scope, global.into(), args.as_slice());

    if let Some(result) = result {
        let result = serde_v8_to_rkyv(scope, result).unwrap();
        Ok(result)
    } else {
        Err(anyhow::Error::msg("Failure".to_string()))
    }

}


#[op2]
#[serde]
fn op_set_globals<'scope>(
    scope: &mut v8::HandleScope<'scope>,
    mut state: &mut OpState,
) -> Result<RkyvSerializedValue, AnyError> {
    // Get the global object
    let global = scope.get_current_context().global(scope);

    let mut my_op_state: &Arc<Mutex<MyOpState>> = state.borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();

    // put globals into the global scope before invoking
    if let RkyvSerializedValue::Object(ref payload_map) = my_op_state.payload {
        if let Some(RkyvSerializedValue::Object(globals_map)) = payload_map.get("globals") {
            for (key, value) in globals_map {
                let key = deno_core::v8::String::new(scope, key).unwrap();
                if let Ok(value) = match deno_core::_ops::RustToV8Fallible::to_v8_fallible(
                    deno_core::_ops::RustToV8Marker::<deno_core::_ops::SerdeMarker, _>::from(
                        value,
                    ),
                    scope,
                ) {
                    // Create a new property in the global object
                    Ok(v) => Ok(v),
                    Err(rv_err) => {
                        let msg = deno_core::v8::String::new(scope, &{
                            let res = std::fmt::format(std::format_args!(
                                "{}",
                                deno_core::anyhow::Error::from(rv_err)
                            ));
                            res
                        })
                            .unwrap();
                        let exc = deno_core::v8::Exception::type_error(scope, msg);
                        scope.throw_exception(exc);
                        Err("Failure".to_string())
                    }
                } {
                    global.set(scope, key.into(), value.into());
                }
            }
        }
    }

    // create shims for functions that are referred to
    // TODO: differentiate async vs sync functions
    let mut js_code = String::new();
    for (function_name, function) in
    create_function_shims(&my_op_state.execution_state_handle, &my_op_state.cell_depended_values).unwrap()
    {
        my_op_state
            .functions
            .insert(function_name.clone(), function);
        js_code.push_str(&format!(
            "globalThis.{function_name} = async (...data) => await op_call_rust(\"{function_name}\", data, {});\n",
            function_name = function_name
        ));
    }

    // Execute the JavaScript code to define the function on the global scope
    let code = v8::String::new(scope, &js_code).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();
    Ok(RkyvSerializedValue::String("Success".to_string()))
}


// Operation to set global variables

type InternalClosureFnMut = Box<
    dyn FnMut(
        Vec<RkyvSerializedValue>,
        Option<HashMap<String, RkyvSerializedValue>>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RkyvSerializedValue>> + Send>> + Send
>;

fn create_function_shims(
    execution_state_handle: &Arc<Mutex<ExecutionState>>,
    cell_depended_values: &HashMap<String, String>,
) -> Result<Vec<(String, InternalClosureFnMut)>, ()> {
    let mut functions = Vec::new();
    // Create shims for functions that are referred to, we look at what functions are being provided
    // and create shims for matches between the function name provided and the identifiers referred to.
    let function_names = {
        let execution_state_handle = execution_state_handle.clone();
        let mut exec_state = execution_state_handle.lock().unwrap();
        exec_state.function_name_to_metadata.keys().cloned().collect::<Vec<_>>()
    };
    for function_name in function_names {
        if cell_depended_values.contains_key(&function_name) {
            let clone_function_name = function_name.clone();
            let execution_state_handle = execution_state_handle.clone();
            let closure_callable: InternalClosureFnMut  = Box::new(move |args: Vec<RkyvSerializedValue>, kwargs: Option<HashMap<String, RkyvSerializedValue>>| {
                let clone_function_name = clone_function_name.clone();
                let execution_state_handle = execution_state_handle.clone();
                async move {
                    let total_arg_payload = js_args_to_rkyv(args, kwargs);
                    let mut new_exec_state = {
                        let mut exec_state = execution_state_handle.lock().unwrap();
                        let mut v = exec_state.clone();
                        std::mem::swap(&mut *exec_state, &mut v);
                        v
                    };
                    let (result, execution_state) = new_exec_state.dispatch(&clone_function_name, total_arg_payload).await?;
                    return Ok(result);
                }.boxed()
            });
            functions.push((function_name.clone(), closure_callable));
        }
    }
    Ok(functions)
}

fn js_args_to_rkyv(args: Vec<RkyvSerializedValue>, kwargs: Option<HashMap<String, RkyvSerializedValue>>) -> RkyvSerializedValue {
    let total_arg_payload = RkyvObjectBuilder::new();
    let total_arg_payload = total_arg_payload.insert_value("args", {
        let mut m = HashMap::new();
        for (i, a) in args.iter().enumerate() {
            m.insert(format!("{}", i), a.clone());
        }
        RkyvSerializedValue::Object(m)
    });

    let total_arg_payload = if let Some(kwargs) = kwargs {
        total_arg_payload.insert_value("kwargs", {
            let mut m = HashMap::new();
            for (k, a) in kwargs.iter() {
                m.insert(k.clone(), a.clone());
            }
            RkyvSerializedValue::Object(m)
        })
    } else {
        total_arg_payload
    };
    total_arg_payload.build()
}


pub async fn source_code_run_deno(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(
    crate::execution::primitives::serialized_value::RkyvSerializedValue,
    Vec<String>,
)> {
    let dependencies = extract_dependencies_js(&source_code);
    let report = build_report(&dependencies);

    // A list of function names this block of code is depending on existing
    let mut cell_depended_values = HashMap::new();
    report.cell_depended_values.iter().for_each(|(k, _)| {
        cell_depended_values.insert(k.clone(), k.clone());
    });

    let execution_state_handle = Arc::new(Mutex::new(execution_state.clone()));
    // TODO: this needs to capture stdout similar to how we do with python
    let my_op_state = Arc::new(Mutex::new(MyOpState {
        output: None,
        payload: payload.clone(),
        cell_depended_values,
        functions: Default::default(),
        execution_state_handle
    }));

    let my_op_state_clone = my_op_state.clone();
    let ext = Extension {
        name: "my_ext",
        ops: std::borrow::Cow::Borrowed(&[
            op_set_globals::DECL,
            op_call_rust::DECL,
            op_assert_eq::DECL,
            op_save_result::DECL,
            op_save_result_object::DECL,
            op_invoke_function::DECL
        ]),
        op_state_fn: Some(Box::new(move |state| {
            state.put(my_op_state_clone);
        })),
        js_files: {
            const JS: &'static [::deno_core::ExtensionFileSource] = &[
                ::deno_core::ExtensionFileSource::loaded_from_memory_during_snapshot("ext:bench_setup/setup.js", {
                    const C: ::deno_core::v8::OneByteConst =
                        ::deno_core::FastStaticString::create_external_onebyte_const(r#"
      ((globalThis) => {
          const { core } = Deno;
          const { ops } = core;
          const op_call_rust = Deno.core.ops.op_call_rust;
          const op_save_result_object =  Deno.core.ops.op_save_result_object;
          const op_save_result =  Deno.core.ops.op_save_result;
          const op_invoke_function =  Deno.core.ops.op_invoke_function;

          globalThis.op_invoke_function = op_invoke_function;
          globalThis.op_call_rust = op_call_rust;

          function argsToMessage(...args) {
              return args.map((arg) => JSON.stringify(arg)).join(" ");
          }

          globalThis.console = {
              log: (...args) => {
              core.print(`[out]: ${argsToMessage(...args)}\n`, false);
              },
              error: (...args) => {
              core.print(`[err]: ${argsToMessage(...args)}\n`, true);
              },
          };

          globalThis.Chidori = {
              assertEq: (a, b) => {
                  return ops.op_assert_eq(a, b);
              },
              saveValue: (val) => {
                  op_save_result(val);
              },
              saveOutput: (object) => {
                  op_save_result_object(object);
              }
          };

          globalThis.module = {
              exports: {}
          };

          ops.op_set_globals();
      })(globalThis);"#.as_bytes());
                    unsafe { std::mem::transmute::<_, ::deno_core::FastStaticString>(&C) }
                })
            ];
            ::std::borrow::Cow::Borrowed(JS)
        },
        ..Default::default()
    };

    // let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // let snapshot_path = out_dir.join("RUNJS_SNAPSHOT.bin");
    // let snapshot = deno_core::snapshot_util::create_snapshot(
    //     deno_core::snapshot_util::CreateSnapshotOptions {
    //         cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
    //         snapshot_path,
    //         startup_snapshot: None,
    //         skip_op_registration: false,
    //         extensions: vec![ext],
    //         compression_cb: None,
    //         with_runtime_cb: None,
    //     }
    // );


    // Set global variables, provide specialized ops
    let source = if let Some(func_name) = function_invocation {
        let mut source = String::new();
        source.push_str("\n");
        source.push_str(&source_code);
        source.push_str("\n");
        source.push_str(&format!(
            r#"Chidori.saveValue(op_invoke_function({name}));"#,
            name = func_name
        ));
        source.push_str("\n");
        source
    } else {
        let mut source = String::new();
        source.push_str("\n");
        source.push_str("export const chidoriResult = {};");
        source.push_str("\n");
        source.push_str(&source_code);
        for (name, report_item) in &report.cell_exposed_values {
            source.push_str("\n");
            source.push_str(&format!(
                r#"chidoriResult["{name}"] = {name};"#,
                name = name
            ));
            source.push_str("\n");
        }
        source.push_str("\n");
        source.push_str("Chidori.saveOutput(chidoriResult);");
        source.push_str("\n");
        source
    };


    let flags = deno::args::Flags::default();
    let factory = deno::factory::CliFactory::from_flags(flags)?;
    let cli_options = factory.cli_options();
    let file_fetcher = factory.file_fetcher()?;
    let main_module = cli_options.resolve_main_module()?;

    // Save a fake file into file fetcher cache
    // to allow module access by TS compiler.
    file_fetcher.insert_memory_files(File {
        specifier: main_module.clone(),
        maybe_headers: None,
        source: source.clone().into_bytes().into(),
    });

    let permissions = PermissionsContainer::new(Permissions::from_options(
        &cli_options.permissions_options(),
    )?);
    // TODO: add custom extensions
    {
        let worker_factory = factory.create_cli_main_worker_factory().await?;
        let mut worker = worker_factory
            .create_custom_worker(
                main_module,
                permissions,
                vec![ext],
                Default::default(),
            )
            .await?;
        let exit_code = worker.run().await?;
    }
    let mut my_op_state = my_op_state.lock().unwrap();
    Ok((my_op_state.output.clone().unwrap_or(RkyvSerializedValue::Null), vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::{SupportedLanguage, TextRange};
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;

    #[tokio::test]
    async fn test_source_code_run_with_external_function_invocation() -> anyhow::Result<()> {
        let source_code = String::from(r#"const y = await test_function(5, 5);"#);

        let mut state = ExecutionState::new();
        let (state, _) = state.update_op(CellTypes::Code(
            crate::cells::CodeCell {
                name: None,
                language: SupportedLanguage::PyO3,
                source_code: String::from(indoc! { r#"
                                    def test_function(a, b):
                                        return a + b
                                "#
                                }),
                function_invocation: None,
            }, TextRange::default()), Some(0))?;
        let result = source_code_run_deno(
            &state,
            &source_code,
            &RkyvObjectBuilder::new()
                .build(),
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("y", 10).build(),
                vec![]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_source_code_run_globals_set_by_payload() {
        let source_code = String::from("const z = a + b;");
        let result = source_code_run_deno(
            &ExecutionState::new(),
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object(
                    "globals",
                    RkyvObjectBuilder::new()
                        .insert_number("a", 20)
                        .insert_number("b", 5),
                )
                .build(),
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("z", 25).build(),
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_source_code_run_deno_success() {
        let source_code = String::from("const x = 42;");
        let result = source_code_run_deno(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("x", 42).build(),
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_source_code_run_deno_failure() {
        let source_code = String::from("throw new Error('Test Error');");
        let result = source_code_run_deno(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_source_code_run_deno_json_serialization() {
        let source_code = String::from("const obj  = {foo: 'bar'};");
        let result = source_code_run_deno(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new()
                    .insert_object(
                        "obj",
                        RkyvObjectBuilder::new().insert_string("foo", "bar".to_string())
                    )
                    .build(),
                vec![]
            )
        );
    }
    #[tokio::test]
    async fn test_source_code_run_deno_expose_global_variables() {
        let source_code = String::from("const x = 30;");
        let result = source_code_run_deno(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("x", 30).build(),
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_function_invocation() {
        let source_code = String::from("function demonstrationAdd(a, b) { return a + b }");
        let args = RkyvObjectBuilder::new()
            .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 10).insert_number("1", 20))
            .build();
        let result = source_code_run_deno(&ExecutionState::new(), &source_code, &args, &Some("demonstrationAdd".to_string())).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvSerializedValue::Number(30),
                vec![]
            )
        );
    }
}
