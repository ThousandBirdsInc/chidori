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

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use tokio::sync::{mpsc, Mutex};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

// Return a global tokio runtime or create one if it doesn't exist.
// Throws a JavaScript exception if the `Runtime` fails to create.
// TODO: note that oncecell has been recently stablized in rust stdlib, so we can probably use that instead
fn runtime<'a, C: Context<'a>>(cx: &mut C) -> NeonResult<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();
    RUNTIME.get_or_try_init(|| Runtime::new().or_else(|err| cx.throw_error(err.to_string())))
}

// impl Serialize for RkyvSerializedValue {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: Serializer,
//     {
//         match self {
//             RkyvSerializedValue::Float(x) => serializer.serialize_f64(*x as f64),
//             RkyvSerializedValue::Number(x) => serializer.serialize_f64(*x as f64),
//             RkyvSerializedValue::String(x) => serializer.serialize_str(x),
//             RkyvSerializedValue::Boolean(x) => serializer.serialize_bool(*x),
//             RkyvSerializedValue::Null => serializer.serialize_unit(),
//             RkyvSerializedValue::Array(val) => {
//                 let mut seq = serializer.serialize_seq(Some(val.len()))?;
//                 for element in val {
//                     seq.serialize_element(element)?;
//                 }
//                 seq.end()
//             }
//             RkyvSerializedValue::Object(val) => {
//                 let mut map = serializer.serialize_map(Some(val.len()))?;
//                 for (key, value) in val {
//                     map.serialize_entry(key, value)?;
//                 }
//                 map.end()
//             }
//             RkyvSerializedValue::StreamPointer(_x) => {
//                 // Handle serialization for StreamPointer
//                 Err(serde::ser::Error::custom(
//                     "StreamPointer serialization not implemented",
//                 ))
//             }
//             RkyvSerializedValue::Cell(_) => {
//                 // Handle serialization for FunctionPointer
//                 Err(serde::ser::Error::custom(
//                     "Cell serialization not implemented",
//                 ))
//             }
//             RkyvSerializedValue::FunctionPointer(_, _) => {
//                 // Handle serialization for FunctionPointer
//                 Err(serde::ser::Error::custom(
//                     "FunctionPointer serialization not implemented",
//                 ))
//             }
//         }
//     }
// }

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
            RkyvSerializedValue::Cell(_) => {
                // Handle serialization for FunctionPointer
                unreachable!();
            }
            // Additional cases for the new enum variants
            RkyvSerializedValue::StreamPointer(_x) => {
                // Convert to JavaScript value as needed
                unreachable!();
            }
            RkyvSerializedValue::FunctionPointer(_, _) => {
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

struct NodeChidori {
    c: Arc<Mutex<String>>,
}

impl Finalize for NodeChidori {}

impl NodeChidori {
    fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<NodeChidori>> {
        Ok(cx.boxed(NodeChidori {
            c: Arc::new(Mutex::new(String::new())),
        }))
    }

    fn push_cell(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        let mut this = cx.this();
        let c = cx.argument::<JsString>(0)?.value(&mut cx);
        let guard = cx.lock();
        // let mut this = this.borrow_mut(&guard);
        // this.c.lock().unwrap().push_str(&c);
        Ok(cx.undefined())
    }

    fn execute(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        let mut this = cx.this();
        let c = cx.argument::<JsString>(0)?.value(&mut cx);
        let guard = cx.lock();
        // let mut this = this.borrow_mut(&guard);
        // this.c.lock().unwrap().push_str(&c);
        Ok(cx.undefined())
    }
}

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
fn std_ai_llm_openai_stream(mut cx: FunctionContext) -> JsResult<JsPromise> {
    let api_key = cx.argument::<JsString>(0)?.value(&mut cx);
    let arg1 = cx.argument::<JsValue>(1)?;
    let arg1_value = match neon_serde3::from_value(&mut cx, arg1) {
        Ok(value) => value,
        Err(e) => {
            return cx.throw_error("Failed to parse".to_string());
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
        deferred.settle_with((&channel), move |mut cx| match result {
            Ok(x) => {
                let js_value = neon_serde3::to_value(&mut cx, &x)
                    .or_else(|e| cx.throw_error(e.to_string()))
                    .unwrap();
                Ok(js_value)
            }
            Err(x) => cx.throw_error(x.to_string()),
        });
    });
    Ok(promise)
}

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
        deferred.settle_with((&channel), move |mut cx| match result {
            Ok(x) => {
                let js_value = neon_serde3::to_value(&mut cx, &x)
                    .or_else(|e| cx.throw_error(e.to_string()))
                    .unwrap();
                Ok(js_value)
            }
            Err(x) => cx.throw_error(x.to_string()),
        });
    });
    Ok(promise)
}

fn std_code_rustpython_source_code_run_python(mut cx: FunctionContext) -> JsResult<JsValue> {
    let source_code = cx.argument::<JsString>(0)?.value(&mut cx);
    match library::std::code::runtime_rustpython::source_code_run_python(
        &source_code,
        &RkyvSerializedValue::Null,
        &None,
    ) {
        Ok(x) => neon_serde3::to_value(&mut cx, &x).or_else(|e| cx.throw_error(e.to_string())),
        Err(x) => cx.throw_error(x.to_string()),
    }
}

fn get_version(mut cx: FunctionContext) -> JsResult<JsValue> {
    Ok(cx.string("0.1.27").upcast())
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    env_logger::init();
    cx.export_function("std_ai_llm_openai_batch", std_ai_llm_openai_batch)?;
    cx.export_function(
        "std_code_rustpython_source_code_run_python",
        std_code_rustpython_source_code_run_python,
    )?;
    cx.export_function("get_version", get_version)?;
    Ok(())
}
