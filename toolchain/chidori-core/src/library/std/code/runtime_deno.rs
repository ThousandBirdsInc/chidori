use std::cell::RefCell;
use anyhow::Result;
use deno_core::error::AnyError;
use deno_core::{Extension, ExtensionFileSource, ExtensionFileSourceCode, FastString, JsRuntime, ModuleSpecifier, Op, op2, OpState, PollEventLoopOptions, RuntimeOptions, serde_json, serde_v8, v8};
use deno_ast::MediaType;
use deno_ast::ParseParams;
use deno_ast::SourceTextInfo;
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
use futures_util::FutureExt;
use tokio::runtime::Runtime;
use crate::cells::{CellTypes, CodeCell};
use crate::execution::execution::ExecutionState;

// TODO: need to override console.log
// TODO: https://deno.com/blog/roll-your-own-javascript-runtime-pt2
// TODO: validate suspension and resumption of execution based on a method that we provide

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
    payload: RkyvSerializedValue,
    cell_depended_values: HashMap<String, String>,
    functions: HashMap<
        String,
        InternalClosureFnMut,
    >,
}

impl MyOpState {
    fn new(payload: RkyvSerializedValue) -> Self {
        MyOpState {
            payload,
            cell_depended_values: Default::default(),
            functions: HashMap::new(),
        }
    }
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
    let result = func(args, kwargs).await;
    Ok(result)
}


struct TsModuleLoader;

impl deno_core::ModuleLoader for TsModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<deno_core::ModuleSpecifier, AnyError> {
        deno_core::resolve_import(specifier, referrer).map_err(|e| e.into())
    }

    fn load(
        &self,
        module_specifier: &deno_core::ModuleSpecifier,
        _maybe_referrer: Option<&reqwest::Url>,
        _is_dyn_import: bool,
    ) -> std::pin::Pin<Box<deno_core::ModuleSourceFuture>> {
        let module_specifier = module_specifier.clone();
        async move {
            let path = module_specifier.to_file_path().unwrap();

            let media_type = MediaType::from_path(&path);
            let (module_type, should_transpile) = match MediaType::from_path(&path) {
                MediaType::JavaScript | MediaType::Mjs | MediaType::Cjs => {
                    (deno_core::ModuleType::JavaScript, false)
                }
                MediaType::Jsx => (deno_core::ModuleType::JavaScript, true),
                MediaType::TypeScript
                | MediaType::Mts
                | MediaType::Cts
                | MediaType::Dts
                | MediaType::Dmts
                | MediaType::Dcts
                | MediaType::Tsx => (deno_core::ModuleType::JavaScript, true),
                MediaType::Json => (deno_core::ModuleType::Json, false),
                _ => panic!("Unknown extension {:?}", path.extension()),
            };

            let code = std::fs::read_to_string(&path)?;
            let code = if should_transpile {
                let parsed = deno_ast::parse_module(ParseParams {
                    specifier: module_specifier.to_string().parse().unwrap(),
                    text_info: SourceTextInfo::from_string(code),
                    media_type,
                    capture_tokens: false,
                    scope_analysis: false,
                    maybe_syntax: None,
                })?;
                parsed.transpile(&Default::default())?.text
            } else {
                code
            };
            let module = deno_core::ModuleSource::new(
                module_type,
                deno_core::ModuleCode::from(code),
                &module_specifier
            );
            Ok(module)
        }
            .boxed_local()
    }
}

// Operation to set global variables
#[allow(non_camel_case_types)]
struct op_set_globals {
    _unconstructable: ::std::marker::PhantomData<()>,
}
impl deno_core::_ops::Op for op_set_globals {
    const NAME: &'static str = "op_set_globals";
    const DECL: deno_core::_ops::OpDecl = deno_core::_ops::OpDecl::new_internal_op2(
        "op_set_globals",
        false,
        false,
        2usize as u8,
        Self::v8_fn_ptr as _,
        Self::v8_fn_ptr_metrics as _,
        None,
        None,
    );
}
impl op_set_globals {
    pub const fn name() -> &'static str {
        "op_set_globals"
    }
    #[deprecated(note = "Use the const op::DECL instead")]
    pub const fn decl() -> deno_core::_ops::OpDecl {
        <Self as deno_core::_ops::Op>::DECL
    }
    #[inline(always)]
    fn slow_function_impl(info: *const deno_core::v8::FunctionCallbackInfo) -> usize {
        #[cfg(debug_assertions)]
        let _reentrancy_check_guard =
            deno_core::_ops::reentrancy_check(&<Self as deno_core::_ops::Op>::DECL);
        let mut scope = unsafe { deno_core::v8::CallbackScope::new(&*info) };
        let mut rv = deno_core::v8::ReturnValue::from_function_callback_info(unsafe { &*info });
        let args = deno_core::v8::FunctionCallbackArguments::from_function_callback_info(unsafe {
            &*info
        });
        let opctx = unsafe {
            &*(deno_core::v8::Local::<deno_core::v8::External>::cast(args.data()).value()
                as *const deno_core::_ops::OpCtx)
        };
        let opstate = &opctx.state;
        let result = {
            let arg0 = &mut scope;
            let arg1 = &mut ::std::cell::RefCell::borrow_mut(&opstate);
            Self::call(arg0, arg1, info)
        };
        rv.set(deno_core::_ops::RustToV8NoScope::to_v8(result));
        return 0;
    }
    extern "C" fn v8_fn_ptr(info: *const deno_core::v8::FunctionCallbackInfo) {
        Self::slow_function_impl(info);
    }
    extern "C" fn v8_fn_ptr_metrics(info: *const deno_core::v8::FunctionCallbackInfo) {
        let args = deno_core::v8::FunctionCallbackArguments::from_function_callback_info(unsafe {
            &*info
        });
        let opctx = unsafe {
            &*(deno_core::v8::Local::<deno_core::v8::External>::cast(args.data()).value()
                as *const deno_core::_ops::OpCtx)
        };
        deno_core::_ops::dispatch_metrics_slow(&opctx, deno_core::_ops::OpMetricsEvent::Dispatched);
        let res = Self::slow_function_impl(info);
        if res == 0 {
            deno_core::_ops::dispatch_metrics_slow(
                &opctx,
                deno_core::_ops::OpMetricsEvent::Completed,
            );
        } else {
            deno_core::_ops::dispatch_metrics_slow(&opctx, deno_core::_ops::OpMetricsEvent::Error);
        }
    }

    #[inline(always)]
    fn call<'a>(
        scope: &mut v8::HandleScope<'a>,
        mut state: &mut OpState,
        info: *const deno_core::v8::FunctionCallbackInfo,
    ) -> v8::Local<'a, v8::String> {
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
        let cell_depended_values = my_op_state.cell_depended_values.clone();
        for (function_name, function) in
            create_function_shims(&my_op_state.payload, cell_depended_values).unwrap()
        {
            my_op_state
                .functions
                .insert(function_name.clone(), function);
            js_code.push_str(&format!(
                "globalThis.{function_name} = async (...data) => await Deno.core.ops.op_call_rust(\"{function_name}\", data, {});\n",
                function_name = function_name
            ));
        }

        // Execute the JavaScript code to define the function on the global scope
        let code = v8::String::new(scope, &js_code).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();

        v8::String::new(scope, &"Success".to_string()).unwrap()
    }
}

type InternalClosureFnMut = Box<
    dyn FnMut(
        Vec<RkyvSerializedValue>,
        Option<HashMap<String, RkyvSerializedValue>>,
    ) -> Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>> + Send
>;

fn create_function_shims(
    payload: &RkyvSerializedValue,
    cell_depended_values: HashMap<String, String>,
) -> Result<Vec<(String, InternalClosureFnMut)>, ()> {
    let mut functions = Vec::new();
    // Create shims for functions that are referred to, we look at what functions are being provided
    // and create shims for matches between the function name provided and the identifiers referred to.
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {
            for (function_name, value) in functions_map {
                let clone_function_name = function_name.clone();
                // TODO: handle llm cells invoked as functions by name
                if let RkyvSerializedValue::Cell(cell) = value.clone() {
                    if let CellTypes::Code(CodeCell {
                        function_invocation,
                        ..
                    }) = &cell
                    {
                        if cell_depended_values.contains_key(&clone_function_name) {
                            let closure_callable: InternalClosureFnMut  = Box::new(move |args: Vec<RkyvSerializedValue>, kwargs: Option<HashMap<String, RkyvSerializedValue>>| {
                                let cell = cell.clone();
                                let clone_function_name = clone_function_name.clone();
                                async move {
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

                                    // modify code cell to indicate execution of the target function
                                    // reconstruction of the cell
                                    let mut op = match &cell {
                                        CellTypes::Code(c) => {
                                            let mut c = c.clone();
                                            c.function_invocation = Some(clone_function_name.clone());
                                            crate::cells::code_cell::code_cell(&c)
                                        }
                                        CellTypes::Prompt(c) => crate::cells::llm_prompt_cell::llm_prompt_cell(&c),

                                        _ => {
                                            unreachable!("Unsupported cell type");
                                        }
                                    };

                                    // invocation of the operation
                                    let result = op.execute(&ExecutionState::new(), total_arg_payload.build(), None).await;
                                    return result;
                                }.boxed()
                            });
                            functions.push((function_name.clone(), closure_callable));
                        }
                    }
                }
            }
        }
    }
    Ok(functions)
}

pub async fn source_code_run_deno(
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
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {
            for (function_name, value) in functions_map {
                cell_depended_values.insert(function_name.clone(), function_name.clone());
            }
        }
    }

    // TODO: this needs to capture stdout similar to how we do with python
    let my_op_state = Arc::new(Mutex::new(MyOpState {
        payload: payload.clone(),
        cell_depended_values: cell_depended_values,
        functions: Default::default(),
    }));

    let ext = Extension {
        name: "my_ext",
        ops: std::borrow::Cow::Borrowed(&[
            op_set_globals::DECL,
            op_call_rust::DECL,
            op_assert_eq::DECL,
        ]),
        op_state_fn: Some(Box::new(move |state| {
            state.put(my_op_state.clone());
        })),
        js_files: std::borrow::Cow::Borrowed(&[
            ExtensionFileSource {
                specifier: "",
                code: ExtensionFileSourceCode::IncludedInBinary(r#"
((globalThis) => {
  const { core } = Deno;
  const { ops } = core;

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
  };

  globalThis.module = {
    exports: {}
  };

  ops.op_set_globals();
})(globalThis);"#, )
            }
        ]),
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

    let mut runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
        module_loader: Some(Rc::new(TsModuleLoader)),
        // startup_snapshot: Some(snapshot),
        extensions: vec![ext],
        ..Default::default()
    });


    // Set global variables, provide specialized ops
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
    source.push_str("chidoriResult;");
    source.push_str("\n");
    dbg!(&source);
    let mod_id = runtime.load_main_module(&ModuleSpecifier::parse("https://localhost/main.js")?, Some(FastString::from(source.to_string()))).await?;
    dbg!(&mod_id);
    let result = runtime.mod_evaluate(mod_id);
    let err = runtime.run_event_loop(PollEventLoopOptions::default()).await;
    if let Err(ref e) = err {
        dbg!("{:?} {:?}", source_code, e);
    }
    // wait for module to resolve
    let resolve = result.await?;
    dbg!(&resolve);

    let global = runtime.get_module_namespace(mod_id).unwrap();
    let scope = &mut runtime.handle_scope();
    let local_var = deno_core::v8::Local::new(scope, global);

    let func_key = v8::String::new(scope, "chidoriResult").unwrap();
    let result = local_var.get(scope, func_key.into()).unwrap();

    let deserialized_value = serde_v8::from_v8::<serde_json::Value>(scope, result);
    dbg!(&deserialized_value);
    return Ok(if let Ok(value) = deserialized_value {
        (json_value_to_serialized_value(&value), vec![])
    } else {
        (RkyvSerializedValue::Null, vec![])
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::SupportedLanguage;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;

    #[tokio::test]
    async fn test_source_code_run_with_external_function_invocation() {
        let source_code = String::from(r#"const y = await test_function(5, 5);"#);
        let result = source_code_run_deno(
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object(
                    "functions",
                    RkyvObjectBuilder::new().insert_value(
                        "test_function",
                        RkyvSerializedValue::Cell(CellTypes::Code(
                            crate::cells::CodeCell {
                                name: None,
                                language: SupportedLanguage::PyO3,
                                source_code: String::from(indoc! { r#"
                                    def test_function(a, b):
                                        return a + b
                                "#
                                }),
                                function_invocation: None,
                            },
                        )),
                    ),
                )
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
    }

    #[tokio::test]
    async fn test_source_code_run_globals_set_by_payload() {
        let source_code = String::from("const z = a + b;");
        let result = source_code_run_deno(
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
        let result = source_code_run_deno(&source_code, &RkyvSerializedValue::Null, &None).await;
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
        let result = source_code_run_deno(&source_code, &RkyvSerializedValue::Null, &None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_source_code_run_deno_json_serialization() {
        let source_code = String::from("const obj  = {foo: 'bar'};");
        let result = source_code_run_deno(&source_code, &RkyvSerializedValue::Null, &None).await;
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
        let result = source_code_run_deno(&source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("x", 30).build(),
                vec![]
            )
        );
    }
}
