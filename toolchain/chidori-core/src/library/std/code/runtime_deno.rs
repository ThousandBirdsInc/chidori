use std::cell::RefCell;
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
use tokio::runtime::{Builder, Runtime};
use tracing::Span;
use crate::cells::{CellTypes, CodeCell, LLMPromptCell};
use crate::execution::execution::execution_state::ExecutionStateErrors;
use crate::execution::execution::ExecutionState;


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
    parent_span_id: Option<tracing::Id>,
    output: Option<RkyvSerializedValue>,
    payload: RkyvSerializedValue,
    cell_depended_values: HashMap<String, String>,
    execution_state_handle: Arc<Mutex<ExecutionState>>,
    stdout: Vec<String>,
    stderr: Vec<String>,
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

#[op2]
#[serde]
fn op_console_log(
    state: Rc<RefCell<OpState>>,
    #[string] message: String,
) -> Result<(), AnyError> {
    let mut op_state = state.borrow_mut();
    let mut my_op_state: &mut Arc<Mutex<MyOpState>> = (*op_state).borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    my_op_state.stdout.push(message.clone());
    println!("[Custom console.log] {:?}", message);
    Ok(())
}

#[op2]
#[serde]
fn op_console_err(
    state: Rc<RefCell<OpState>>,
    #[string] message: String,
) -> Result<(), AnyError> {
    let mut op_state = state.borrow_mut();
    let mut my_op_state: &mut Arc<Mutex<MyOpState>> = (*op_state).borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    my_op_state.stderr.push(message.clone());
    println!("[Custom console.err] {:?}", message);
    Ok(())
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

// TODO: template to implement array storage
// #[op2]
// fn op_transfer_arraybuffer<'a>(
//     scope: &mut v8::HandleScope<'a>,
//     ab: &v8::ArrayBuffer,
// ) -> Result<v8::Local<'a, v8::ArrayBuffer>, AnyError> {
//     if !ab.is_detachable() {
//         return Err(type_error("ArrayBuffer is not detachable"));
//     }
//     let bs = ab.get_backing_store();
//     ab.detach(None);
//     Ok(v8::ArrayBuffer::with_backing_store(scope, &bs))
// }

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
    create_function_shims(&my_op_state.execution_state_handle, &my_op_state.cell_depended_values, my_op_state.parent_span_id.clone()).unwrap()
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
    ) -> Pin<Box<dyn Future<Output = Result<RkyvSerializedValue, ExecutionStateErrors>> + Send>> + Send
>;

fn create_function_shims(
    execution_state_handle: &Arc<Mutex<ExecutionState>>,
    cell_depended_values: &HashMap<String, String>,
    parent_span_id: Option<tracing::Id>,
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
            let parent_span_id = parent_span_id.clone();
            let closure_callable: InternalClosureFnMut  = Box::new(move |args: Vec<RkyvSerializedValue>, kwargs: Option<HashMap<String, RkyvSerializedValue>>| {
                let clone_function_name = clone_function_name.clone();
                let execution_state_handle = execution_state_handle.clone();
                let parent_span_id = parent_span_id.clone();
                async move {
                    let total_arg_payload = js_args_to_rkyv(args, kwargs);
                    let mut new_exec_state = {
                        let mut exec_state = execution_state_handle.lock().unwrap();
                        let mut v = exec_state.clone();
                        // get a new execution state
                        std::mem::swap(&mut *exec_state, &mut v);
                        v
                    };
                    println!("Deno expecting to call dispatch");
                    let (result, execution_state) = new_exec_state.dispatch(&clone_function_name, total_arg_payload, parent_span_id.clone()).await?;
                    return result;
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


#[tracing::instrument]
pub async fn source_code_run_deno(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(
    Result<RkyvSerializedValue, ExecutionStateErrors>,
    Vec<String>,
    Vec<String>
)> {
    let execution_state = execution_state.clone();
    let source_code = source_code.clone();
    let function_invocation = function_invocation.clone();
    let payload = payload.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    // let (tx, rx) = tokio::sync::oneshot::channel();

    std::thread::spawn(move || {
        let thread_result = (|| -> anyhow::Result<(
            Result<RkyvSerializedValue, ExecutionStateErrors>,
            Vec<String>,
            Vec<String>,
        )> {
            let source_code = source_code.clone();
            // Capture the current span's ID
            let current_span_id = Span::current().id();

            let dependencies = extract_dependencies_js(&source_code)?;
            let report = build_report(&dependencies);

            // A list of function names this block of code is depending on existing
            let mut cell_depended_values = HashMap::new();
            report.cell_depended_values.iter().for_each(|(k, _)| {
                cell_depended_values.insert(k.clone(), k.clone());
            });

            let execution_state_handle = Arc::new(Mutex::new(execution_state.clone()));
            let my_op_state = Arc::new(Mutex::new(MyOpState {
                parent_span_id: current_span_id,
                stdout: vec![],
                stderr: vec![],
                output: None,
                payload: payload.clone(),
                cell_depended_values,
                functions: Default::default(),
                execution_state_handle
            }));

            let my_op_state_clone = my_op_state.clone();
            let ext = Extension {
                name: "chidori_ext",
                ops: std::borrow::Cow::Borrowed(&[
                    op_set_globals::DECL,
                    op_call_rust::DECL,
                    op_assert_eq::DECL,
                    op_save_result::DECL,
                    op_save_result_object::DECL,
                    op_invoke_function::DECL,
                    op_console_log::DECL,
                    op_console_err::DECL,
                ]),
                op_state_fn: Some(Box::new(move |state| {
                    state.put(my_op_state_clone);
                })),
                js_files: {
                    const JS: &'static [::deno_core::ExtensionFileSource] = &[
                        ::deno_core::ExtensionFileSource::loaded_from_memory_during_snapshot("ext:chidori_setup/setup.js", {
                            const C: ::deno_core::v8::OneByteConst =
                                ::deno_core::FastStaticString::create_external_onebyte_const(r#"
      ((globalThis) => {
          const { core } = Deno;
          const { ops } = core;
          const op_assert_eq = Deno.core.ops.ops_assert_eq;
          const op_call_rust = Deno.core.ops.op_call_rust;
          const op_save_result_object = Deno.core.ops.op_save_result_object;
          const op_save_result = Deno.core.ops.op_save_result;
          const op_invoke_function = Deno.core.ops.op_invoke_function;
          const op_console_log = Deno.core.ops.op_console_log;
          const op_console_err = Deno.core.ops.op_console_err;

          globalThis.op_invoke_function = op_invoke_function;
          globalThis.op_call_rust = op_call_rust;

          function argsToMessage(...args) {
              return args.map((arg) => JSON.stringify(arg)).join(" ");
          }

          globalThis.console = {
              log: (...args) => {
                  op_console_log(`[out]: ${argsToMessage(...args)}\n`);
                  core.print(`[out]: ${argsToMessage(...args)}\n`, false);
              },
              error: (...args) => {
                  op_console_err(`[out]: ${argsToMessage(...args)}\n`);
                  core.print(`[err]: ${argsToMessage(...args)}\n`, true);
              },
          };

          globalThis.Chidori = {
              assertEq: (a, b) => {
                  return a == b;
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
                for (name, report_item) in &report.triggerable_functions {
                    source.push_str("\n");
                    source.push_str(&format!(
                        r#"chidoriResult["{name}"] = "function";"#,
                        name = name
                    ));
                    source.push_str("\n");
                }
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


            let mut flags = deno::args::Flags::default();
            // TODO: give user control over this in configuration
            // TODO: allow_net is causing this to block our execution entirely
            flags.allow_net = Some(vec![]);
            flags.allow_env = Some(vec![]);
            flags.allow_read = Some(vec![]);
            flags.allow_write = Some(vec![]);
            flags.allow_run = Some(vec![]);
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

            // Create a single-threaded runtime
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime");

            // Use the newly created single-threaded runtime to run our async code
            runtime.block_on(async {
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
                Ok::<(), anyhow::Error>(())
            }).map_err(|e| {
                // TODO: map error
                dbg!(&e);
                e
            })?;
            println!("After the runtime block on in source_code_run_deno");

            println!("Attempting to lock my_op_state");
            let mut my_op_state = my_op_state.lock().unwrap();
            println!("After the my_op_state lock");
            let output = Ok(my_op_state.output.clone().unwrap_or(RkyvSerializedValue::Null));
            println!("After the my_op_state lock here is the output {:?}", &output);
            let stdout = my_op_state.stdout.clone();
            println!("After the my_op_state lock here is the stdout {:?}", &stdout);
            let stderr = my_op_state.stderr.clone();
            println!("After the my_op_state lock here is the stderr {:?}", &stderr);
            Ok((output, stdout, stderr))
        })();

        if tx.send(thread_result).is_err() {
            eprintln!("Receiver dropped");
        }
        println!("After sending the tx");
        anyhow::Ok(())
    });

    println!("Awaiting the rx");
    // Receive the result asynchronously without blocking the executor
    let result_of_thread = tokio::task::spawn_blocking(move || {
        println!("Inside spawn_blocking closure");
        rx.recv()
    })
        .await?
        .map_err(|e| anyhow::anyhow!("Failed to receive from channel: {:?}", e))??;

    dbg!(&result_of_thread);
    Ok(result_of_thread)
}

fn replace_identifier(code: &str, old_identifier: &str, new_identifier: &str) -> String {
    let pattern = if old_identifier.starts_with('$') {
        format!(r"(^|[^a-zA-Z0-9_$])({})(?![a-zA-Z0-9_$])", regex::escape(old_identifier))
    } else {
        format!(r"(?<![a-zA-Z0-9_$]){}(?![a-zA-Z0-9_$])", regex::escape(old_identifier))
    };
    let re = fancy_regex::Regex::new(&pattern).unwrap();
    re.replace_all(code, |caps: &fancy_regex::Captures| {
        if old_identifier.starts_with('$') {
            format!("{}{}", caps.get(1).map_or("", |m| m.as_str()), new_identifier)
        } else {
            new_identifier.to_string()
        }
    }).to_string()
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

        let mut state = ExecutionState::new_with_random_id();
        let (state, _) = state.update_operation(CellTypes::Code(
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
                Ok(RkyvObjectBuilder::new().insert_number("y", 10).build()),
                vec![],
                vec![],
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_source_code_run_globals_set_by_payload() {
        let source_code = String::from("const z = a + b;");
        let result = source_code_run_deno(
            &ExecutionState::new_with_random_id(),
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
                Ok(RkyvObjectBuilder::new().insert_number("z", 25).build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_source_code_run_deno_success() {
        let source_code = String::from("const x = 42;");
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("x", 42).build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_source_code_run_deno_failure() {
        let source_code = String::from("throw new Error('Test Error');");
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_source_code_run_deno_json_serialization() {
        let source_code = String::from("const obj  = {foo: 'bar'};");
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new()
                    .insert_object(
                        "obj",
                        RkyvObjectBuilder::new().insert_string("foo", "bar".to_string())
                    )
                    .build()),
                vec![],
                vec![],
            )
        );
    }
    #[tokio::test]
    async fn test_source_code_run_deno_expose_global_variables() {
        let source_code = String::from("const x = 30;");
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("x", 30).build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_function_invocation() {
        let source_code = String::from("function demonstrationAdd(a, b) { return a + b }");
        let args = RkyvObjectBuilder::new()
            .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 10).insert_number("1", 20))
            .build();
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &args, &Some("demonstrationAdd".to_string())).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Number(30)),
                vec![],
                vec![],
            )
        );
    }


    #[tokio::test]
    async fn test_console_log_console_err_behaviors() {
        let source_code = String::from(r#"
        console.log("testing, output");
        console.error("testing, stderr");
        "#);
        let args = RkyvObjectBuilder::new()
            .build();
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &args, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().build()),
                vec![String::from("[out]: \"testing, output\"\n")],
                vec![String::from("[out]: \"testing, stderr\"\n")],
            )
        );
    }

    #[test]
    fn test_identifier_replacement() {
        let test_cases = vec![
            ("x", "y", "function func(x) { return x; }", "function func(y) { return y; }"),
            ("func", "newFunc", "function func() {}; func();", "function newFunc() {}; newFunc();"),
            ("_private", "_hidden", "let _private = 1; let not_private = 2;", "let _hidden = 1; let not_private = 2;"),
            ("MAX_VALUE", "MAXIMUM", "const MAX_VALUE = 100; const MAX_VALUE_LIMIT = 200;", "const MAXIMUM = 100; const MAX_VALUE_LIMIT = 200;"),
            ("i", "index", "for (let i = 0; i < 10; i++) { console.log(i); }", "for (let index = 0; index < 10; index++) { console.log(index); }"),
            ("data", "info", "let data = [1, 2, 3]; let moreData = [4, 5, 6];", "let info = [1, 2, 3]; let moreData = [4, 5, 6];"),
            ("calculate", "compute", "function calculate(x) { return calculate(x-1); }", "function compute(x) { return compute(x-1); }"),
            ("temp", "temporary", "let temp = 98.6; let temperature = 100;", "let temporary = 98.6; let temperature = 100;"),
            ("log", "logger", "import { log } from 'util'; log.info('message');", "import { logger } from 'util'; logger.info('message');"),
            ("String", "Str", "let strValue = String(42);", "let strValue = Str(42);"),
            ("Object", "Dict", "let myObj = new Object(); let obj = {};", "let myObj = new Dict(); let obj = {};"),
            ("console", "logger", "console.log('Hello'); let printer = null;", "logger.log('Hello'); let printer = null;"),
            ("Math", "MathUtils", "Math.sum([1, 2, 3]); let mathOp = x => x;", "MathUtils.sum([1, 2, 3]); let mathOp = x => x;"),
            ("Error", "Exception", "throw new Error('error'); class ErrorHandler {}", "throw new Exception('error'); class ErrorHandler {}"),
            ("$scope", "$state", "function ctrl($scope) { $scope.value = 10; }", "function ctrl($state) { $state.value = 10; }"),
        ];

        for (old, new, input, expected) in test_cases {
            let result = replace_identifier(input, old, new);
            assert_eq!(result, expected, "Failed to replace '{}' with '{}'", old, new);
        }
    }

    #[tokio::test]
    async fn test_typescript_basic() {
        let source_code = String::from("const x: number = 42;");
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("x", 42).build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_typescript_interface() {
        let source_code = String::from(r#"
        interface Person {
            name: string;
            age: number;
        }
        const person: Person = { name: "Alice", age: 30 };
    "#);
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new()
                    .insert_object(
                        "person",
                        RkyvObjectBuilder::new()
                            .insert_string("name", "Alice".to_string())
                            .insert_number("age", 30)
                    )
                    .build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_typescript_generics() {
        let source_code = String::from(r#"
        function identity<T>(arg: T): T {
            return arg;
        }
        const result = identity<string>("TypeScript");
    "#);
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new()
                    .insert_string("identity", "function".to_string())
                    .insert_string("result", "TypeScript".to_string())
                    .build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_typescript_async_await() {
        let source_code = String::from(r#"
        async function fetchData(): Promise<string> {
            return new Promise(resolve => setTimeout(() => resolve("Data"), 100));
        }
        const data = await fetchData();
    "#);
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new()
                    .insert_string("data", "Data".to_string())
                    .insert_string("fetchData", "function".to_string())
                    .build()),
                vec![],
                vec![],
            )
        );
    }

    #[tokio::test]
    async fn test_typescript_enum() {
        let source_code = String::from(r#"
        enum Color {
            Red,
            Green,
            Blue,
        }
        const selectedColor: Color = Color.Green;
    "#);
        let result = source_code_run_deno(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new()
                    .insert_number("selectedColor", 1)
                    .build()),
                vec![],
                vec![],
            )
        );
    }
}
