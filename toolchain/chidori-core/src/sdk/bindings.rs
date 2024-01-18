use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use crate::library;
use crate::library::std::ai::llm::ChatCompletionReq;
use crate::library::std::ai::llm::ChatModelBatch;
use futures::StreamExt;
use neon::prelude::*;
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::runtime::Runtime;

// Return a global tokio runtime or create one if it doesn't exist.
// Throws a JavaScript exception if the `Runtime` fails to create.
// TODO: note that oncecell has been recently stablized in rust stdlib, so we can probably use that instead
fn runtime<'a, C: Context<'a>>(cx: &mut C) -> NeonResult<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();
    RUNTIME.get_or_try_init(|| Runtime::new().or_else(|err| cx.throw_error(err.to_string())))
}

impl RkyvSerializedValue {
    fn to_object<'a, T>(&self, cx: &mut T) -> JsResult<'a, JsValue>
    where
        T: Context<'a>,
    {
        match &self {
            RkyvSerializedValue::Float(x) => Ok(cx.number(*x as f64).upcast()),
            RkyvSerializedValue::Number(x) => Ok(cx.number(*x as f64).upcast()),
            RkyvSerializedValue::String(x) => Ok(cx.string(x).upcast()),
            RkyvSerializedValue::Boolean(x) => Ok(cx.boolean(*x).upcast()),
            RkyvSerializedValue::Null => Ok(cx.null().upcast()),
            RkyvSerializedValue::Array(val) => {
                let js_list = cx.empty_array();
                for (idx, item) in val.iter().enumerate() {
                    let js = item.to_object(cx);
                    js_list.set(cx, idx as u32, js?)?;
                }
                Ok(js_list.upcast())
            }
            RkyvSerializedValue::Object(val) => {
                let js_obj = cx.empty_object();
                for (key, value) in val {
                    let js = value.to_object(cx);
                    js_obj.set(cx, key.as_str(), js?).unwrap();
                }
                Ok(js_obj.upcast())
            }
            // Additional cases for the new enum variants
            RkyvSerializedValue::StreamPointer(_x) => {
                // Convert to JavaScript value as needed
                unreachable!();
            }
            RkyvSerializedValue::FunctionPointer(_x) => {
                // Convert to JavaScript value as needed
                unreachable!();
            }
        }
    }
}

fn from_js_value<'a, C: Context<'a>>(
    cx: &mut C,
    value: Handle<JsValue>,
) -> NeonResult<RkyvSerializedValue> {
    if value.is_a::<JsUndefined, _>(cx) {
        Ok(RkyvSerializedValue::Null)
    } else if let Ok(num) = value.downcast::<JsNumber, _>(cx) {
        Ok(RkyvSerializedValue::Float(num.value(cx) as f32))
    } else if let Ok(bool) = value.downcast::<JsBoolean, _>(cx) {
        Ok(RkyvSerializedValue::Boolean(bool.value(cx)))
    } else if let Ok(str) = value.downcast::<JsString, _>(cx) {
        Ok(RkyvSerializedValue::String(str.value(cx)))
    } else if let Ok(arr) = value.downcast::<JsArray, _>(cx) {
        let mut vals = Vec::new();
        for i in 0..arr.len(cx) {
            let v = arr.get(cx, i)?;
            vals.push(from_js_value(cx, v)?);
        }
        Ok(RkyvSerializedValue::Array(vals))
    } else if let Ok(obj) = value.downcast::<JsObject, _>(cx) {
        let mut vals = HashMap::new();
        for key in obj.get_own_property_names(cx)?.to_vec(cx)? {
            let v = obj.get(cx, key)?;
            let k = key.downcast::<JsString, _>(cx).unwrap();
            vals.insert(k.value(cx), from_js_value(cx, v)?);
        }
        Ok(RkyvSerializedValue::Object(vals))
    } else {
        unreachable!();
        // Handle additional cases as needed, like StreamPointer and FunctionPointer
    }
}

/// ==============================
/// The graph building execution engine is exposed via a stateful object interface
/// ==============================

// struct NodeChidori {
//     c: Arc<Mutex<Chidori>>
// }
//
// impl Finalize for NodeChidori {}
//
// impl NodeChidori {
//     fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<NodeChidori>> {
//         let file_id = cx.argument::<JsString>(0)?.value(&mut cx);
//         let url = cx.argument::<JsString>(1)?.value(&mut cx);
//
//         if !url.contains("://") {
//             return cx.throw_error("Invalid url, must include protocol");
//         }
//         // let api_token = cx.argument_opt(2)?.value(&mut cx);
//         debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, "".to_string());
//         Ok(cx.boxed(NodeChidori {
//             c: Arc::new(Mutex::new(Chidori::new(file_id, url))),
//         }))
//     }
// }

macro_rules! return_or_throw_deferred {
    ($channel:expr, $deferred:expr, $m:expr) => {
        if let Ok(result) = $m {
            $deferred.settle_with($channel, move |mut cx| {
                neon_serde3::to_value(&mut cx, &result).or_else(|e| cx.throw_error(e.to_string()))
            });
        } else {
            $deferred.settle_with($channel, move |mut cx| cx.throw_error("Error playing"));
        }
    };
}

/// ==============================
/// The standard library functions are exposed directly to JavaScript for one-off execution.
/// ==============================

fn std_ai_llm_openai_batch(mut cx: FunctionContext) -> JsResult<JsPromise> {
    let api_key = cx.argument::<JsString>(0)?.value(&mut cx);
    let arg1 = cx.argument::<JsValue>(1)?;
    let arg1_value = match neon_serde3::from_value(&mut cx, arg1) {
        Ok(value) => value,
        Err(e) => {
            return cx.throw_error(e.to_string());
        }
    };

    let channel = cx.channel();
    let (deferred, promise) = cx.promise();
    let rt = runtime(&mut cx)?;
    rt.spawn(async move {
        let result = library::std::ai::llm::openai::OpenAIChatModel::new(api_key)
            .batch(arg1_value)
            .await;
        deferred.settle_with((&channel), move |mut cx| {
            if let Ok(result) = result {
                neon_serde3::to_value(&mut cx, &result).or_else(|e| cx.throw_error(e.to_string()))
            } else {
                cx.throw_error("Error")
            }
        });
    });
    Ok(promise)
}

fn std_code_rustpython_source_code_run_python(mut cx: FunctionContext) -> JsResult<JsValue> {
    let source_code = cx.argument::<JsString>(0)?.value(&mut cx);
    match library::std::code::runtime_rustpython::source_code_run_python(source_code) {
        Ok(x) => x.to_object(&mut cx),
        Err(x) => cx.throw_error(x.to_string()),
    }
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    env_logger::init();
    cx.export_function("std_ai_llm_openai_batch", std_ai_llm_openai_batch)?;
    cx.export_function(
        "std_code_rustpython_source_code_run_python",
        std_code_rustpython_source_code_run_python,
    )?;
    Ok(())
}
