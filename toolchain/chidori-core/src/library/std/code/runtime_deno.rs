use anyhow::Result;
use deno_core::error::AnyError;
use deno_core::serde_json::Value;
use deno_core::{
    extension, op2, ops, serde_json, serde_v8, v8, Extension, FastString, JsRuntime, Op, OpState,
    RuntimeOptions,
};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use crate::execution::primitives::cells::{CellTypes, CodeCell};
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use deno_core::v8::{HandleScope, Local, Object};
use pyo3::types::{PyCFunction, PyDict, PyTuple};
use pyo3::PyResult;
use std::collections::HashMap;
use std::hash::Hash;

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
        Box<
            dyn FnMut(
                Vec<RkyvSerializedValue>,
                Option<HashMap<String, RkyvSerializedValue>>,
            ) -> RkyvSerializedValue,
        >,
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
fn op_call_rust(
    state: &mut OpState,
    #[string] name: String,
    #[serde] args: Vec<RkyvSerializedValue>,
    #[serde] kwargs: HashMap<String, RkyvSerializedValue>,
) -> Result<RkyvSerializedValue, AnyError> {
    let mut my_op_state: &Arc<Mutex<MyOpState>> = state.borrow_mut();
    let mut my_op_state = my_op_state.lock().unwrap();
    let kwargs = if kwargs.is_empty() {
        None
    } else {
        Some(kwargs)
    };
    Ok(my_op_state.functions.get_mut(&name).unwrap()(args, kwargs))
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
        let mut js_code = String::new();
        let cell_depended_values = my_op_state.cell_depended_values.clone();
        for (function_name, function) in
            create_function_shims(scope, &my_op_state.payload, cell_depended_values).unwrap()
        {
            my_op_state
                .functions
                .insert(function_name.clone(), function);
            js_code.push_str(&format!(
                "globalThis.{function_name} = (...data) => Deno.core.ops.op_call_rust(\"test_function\", data, {});\n",
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

fn generic_callback(data: &str) -> String {
    // Process the data and return a response
    format!("Processed by Rust: {}", data)
}

type InternalClosureFnMut = Box<
    dyn FnMut(
        Vec<RkyvSerializedValue>,
        Option<HashMap<String, RkyvSerializedValue>>,
    ) -> RkyvSerializedValue,
>;

fn create_function_shims(
    scope: &mut HandleScope,
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
                            let closure_callable:  InternalClosureFnMut  = Box::new(move |args: Vec<RkyvSerializedValue>, kwargs: Option<HashMap<String, RkyvSerializedValue>>| {
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
                                        crate::cells::code_cell(&c)
                                    }
                                    CellTypes::Prompt(c) => crate::cells::llm_prompt_cell(&c),
                                };

                                // invocation of the operation
                                let result = op.execute(total_arg_payload.build());
                                return result;
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

pub fn source_code_run_deno(
    source_code: String,
    payload: &RkyvSerializedValue,
    function_invocation: Option<String>,
) -> Result<Option<Value>> {
    let mut cell_depended_values = HashMap::new();
    cell_depended_values.insert("test_function".to_string(), "test_function".to_string());

    let my_op_state = Arc::new(Mutex::new(MyOpState {
        payload: payload.clone(),
        cell_depended_values: cell_depended_values,
        functions: Default::default(),
    }));

    // Wrap the callback in a Box<dyn Fn()> and share it across threads safely using Arc<Mutex<>>.
    let callback: Arc<Mutex<Box<dyn Fn(&str) -> String>>> =
        Arc::new(Mutex::new(Box::new(generic_callback)));

    let ext = Extension {
        name: "my_ext",
        ops: std::borrow::Cow::Borrowed(&[op_set_globals::DECL, op_call_rust::DECL]),
        op_state_fn: Some(Box::new(move |state| {
            state.put(my_op_state.clone());
        })),
        ..Default::default()
    };

    // Initialize JS runtime
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![ext],
        ..Default::default()
    });

    // Set global variables, provide specialized ops
    // TODO: like valtown, lets implement our own module loader and make chidori with its ops, a module
    runtime.execute_script(
        "<init>",
        FastString::Static(
            r#"
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

  ops.op_set_globals();
})(globalThis);
        "#,
        ),
    )?;

    // TODO: the script receives the arguments as a json payload "#state"

    // Wrap the source code in an entrypoint function so that it immediately evaluates
    let wrapped_source_code = format!(
        r#"(function main() {{
        {}
    }})();"#,
        source_code
    );

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
    use crate::execution::primitives::cells::SupportedLanguage;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;

    #[test]
    fn test_source_code_run_with_external_function_invocation() {
        let source_code = String::from(r#"return test_function(5, 5);"#);
        let result = source_code_run_deno(
            source_code,
            &RkyvObjectBuilder::new()
                .insert_object(
                    "functions",
                    RkyvObjectBuilder::new().insert_value(
                        "test_function",
                        RkyvSerializedValue::Cell(CellTypes::Code(
                            crate::execution::primitives::cells::CodeCell {
                                language: SupportedLanguage::Python,
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
            None,
        );
        assert_eq!(result.unwrap(), Some(serde_json::json!(10)));
    }

    #[test]
    fn test_source_code_run_globals_set_by_payload() {
        let source_code = String::from(r#"return a + b;"#);
        let result = source_code_run_deno(
            source_code,
            &RkyvObjectBuilder::new()
                .insert_object(
                    "globals",
                    RkyvObjectBuilder::new()
                        .insert_number("a", 20)
                        .insert_number("b", 5),
                )
                .build(),
            None,
        );
        assert_eq!(result.unwrap(), Some(serde_json::json!(25)));
    }

    #[test]
    fn test_source_code_run_deno_success() {
        let source_code = String::from("return 42;");
        let result = source_code_run_deno(source_code, &RkyvSerializedValue::Null, None);
        assert_eq!(result.unwrap(), Some(serde_json::json!(42)));
    }

    #[test]
    fn test_source_code_run_deno_failure() {
        let source_code = String::from("throw new Error('Test Error');");
        let result = source_code_run_deno(source_code, &RkyvSerializedValue::Null, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_source_code_run_deno_json_serialization() {
        let source_code = String::from("return {foo: 'bar'};");
        let result = source_code_run_deno(source_code, &RkyvSerializedValue::Null, None);
        assert_eq!(result.unwrap(), Some(serde_json::json!({"foo": "bar"})));
    }
}
