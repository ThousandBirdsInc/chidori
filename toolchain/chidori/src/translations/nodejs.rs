use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc};
use tokio::sync::{mpsc, Mutex};
use anyhow::Error;
use futures::StreamExt;
use log::{debug, info};
use neon::{prelude::*, types::Deferred};
use neon::handle::Managed;
use neon::result::Throw;
use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;
use prompt_graph_core::build_runtime_graph::graph_parse::{CleanedDefinitionGraph, CleanIndividualNode, construct_query_from_output_type, derive_for_individual_node};
use prompt_graph_core::graph_definition::{create_code_node, create_custom_node, create_prompt_node, create_vector_memory_node, SourceNodeType};
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, Empty, ExecutionStatus, File, FileAddressedChangeValueWithCounter, FilteredPollNodeWillExecuteEventsRequest, Item, ListBranchesRes, Path, Query, QueryAtFrame, QueryAtFrameResponse, RequestAckNodeWillExecuteEvent, RequestAtFrame, RequestFileMerge, RequestListBranches, RequestNewBranch, RequestOnlyId, SerializedValue, SerializedValueArray, SerializedValueObject};
use prompt_graph_core::proto2::execution_runtime_client::ExecutionRuntimeClient;
use prompt_graph_core::proto2::serialized_value::Val;
use prompt_graph_exec::tonic_runtime::run_server;
use neon_serde3;
use prost::bytes::Buf;
use serde::{Deserialize, Serialize};
use crate::translations::rust::{Chidori, CustomNodeCreateOpts, DenoCodeNodeCreateOpts, GraphBuilder, Handler, NodeHandle, PromptNodeCreateOpts, VectorMemoryNodeCreateOpts};

// Return a global tokio runtime or create one if it doesn't exist.
// Throws a JavaScript exception if the `Runtime` fails to create.
// TODO: note that oncecell has been recently stablized in rust stdlib, so we can probably use that instead
fn runtime<'a, C: Context<'a>>(cx: &mut C) -> NeonResult<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();
    RUNTIME.get_or_try_init(|| Runtime::new().or_else(|err| cx.throw_error(err.to_string())))
}


async fn get_client(url: String) -> Result<ExecutionRuntimeClient<tonic::transport::Channel>, tonic::transport::Error> {
    ExecutionRuntimeClient::connect(url.clone()).await
}


#[derive(Debug)]
pub struct SerializedValueWrapper(SerializedValue);

impl SerializedValueWrapper {
    fn to_object<'a, T>(&self, cx: &mut T) -> JsResult<'a, JsValue>  where
    T: Context<'a> {
        if let None = self.0.val {
            let x: Option<bool> = None;
            return Ok(cx.undefined().upcast());
        }
        let result: Handle<JsValue> = match self.0.val.as_ref().unwrap() {
            Val::Float(x) => { cx.number(*x as f64).upcast() }
            Val::Number(x) => { cx.number(*x as f64).upcast() }
            Val::String(x) => { cx.string(x).upcast() }
            Val::Boolean(x) => { cx.boolean(*x).upcast() }
            Val::Array(val) => {
                let mut js_list = cx.empty_array();
                for (idx, item) in val.values.iter().enumerate() {
                    let js = SerializedValueWrapper(item.clone()).to_object(cx);
                    js_list.set(cx, idx as u32, js?)?;
                }
                js_list.upcast()
            }
            Val::Object(val) => {
                let mut js_obj = cx.empty_object();
                for (key, value) in &val.values {
                    let js = SerializedValueWrapper(value.clone()).to_object(cx);
                    js_obj.set(cx, key.as_str(), js?).unwrap();
                }
                js_obj.upcast()
            }
        };
        Ok(result)
    }
}


fn from_js_value<'a, C: Context<'a>>(cx: &mut C, value: Handle<JsValue>) -> NeonResult<SerializedValue> {
    if value.is_a::<JsUndefined, _>(cx) {
        return Ok(SerializedValue { val: None });
    } else if let Ok(num) = value.downcast::<JsNumber, _>(cx) {
        return Ok(SerializedValue { val: Some(Val::Float(num.value(cx) as f32))});
    } else if let Ok(bool) = value.downcast::<JsBoolean, _>(cx) {
        return Ok(SerializedValue { val: Some(Val::Boolean(bool.value(cx)))});
    } else if let Ok(str) = value.downcast::<JsString, _>(cx) {
        return Ok(SerializedValue { val: Some(Val::String(str.value(cx)))});
    } else if let Ok(arr) = value.downcast::<JsArray, _>(cx) {
        let mut vals = Vec::new();
        for i in 0..arr.len(cx) {
            let v = arr.get(cx, i)?;
            vals.push(from_js_value(cx, v)?);
        }
        return Ok(SerializedValue { val: Some(Val::Array(SerializedValueArray { values: vals }))});
    } else if let Ok(obj) = value.downcast::<JsObject, _>(cx) {
        let mut vals = HashMap::new();
        for key in obj.get_own_property_names(cx)?.to_vec(cx)? {
            let v = obj.get(cx, key)?;
            let k = key.downcast::<JsString, _>(cx);
            vals.insert(k.unwrap().value(cx), from_js_value(cx, v)?);
        }
        return Ok(SerializedValue { val: Some(Val::Object(SerializedValueObject { values: vals }))});
    }

    cx.throw_error("Unsupported type")
}

macro_rules! return_or_throw_deferred {
    ($channel:expr, $deferred:expr, $m:expr) => {
        if let Ok(result) = $m {
            $deferred.settle_with($channel, move |mut cx| {
                neon_serde3::to_value(&mut cx, &result)
                    .or_else(|e| cx.throw_error(e.to_string()))
            });
        } else {
            $deferred.settle_with($channel, move |mut cx| {
                cx.throw_error("Error playing")
            });
        }
    };
}


// Node handle
#[derive(Clone)]
pub struct NodeNodeHandle {
    n: NodeHandle
}

impl NodeNodeHandle {
    fn from(n: NodeHandle) -> NodeNodeHandle {
        NodeNodeHandle{ n }
    }
}

impl Finalize for NodeNodeHandle {}


impl NodeNodeHandle {
    fn get_name(&self) -> String {
        self.n.get_name()
    }

    pub fn run_when(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<RefCell<NodeNodeHandle>>, _>(&mut cx)?;

        let graph_builder = cx.argument::<JsBox<NodeGraphBuilder>>(0)?;
        let other_node_handle = cx.argument::<JsBox<RefCell<NodeNodeHandle>>>(1)?;

        let mut n = &mut self_.borrow_mut().n;
        let g = &graph_builder.g;
        let mut graph_builder = g.blocking_lock();
        let other_node = &other_node_handle.borrow().n;
        let m = n.run_when(&mut graph_builder, &other_node);
        deferred.settle_with((&channel), move |mut cx| {
            if let Ok(result) = m {
                Ok(cx.boolean(result))
            } else {
                cx.throw_error("Error playing")
            }
        });
        Ok(promise)

    }

    // pub fn query(mut cx: FunctionContext) -> JsResult<JsPromise> {
    //
    // }
}



fn obj_to_paths<'a, C: Context<'a>>(cx: &mut C, d: Handle<JsObject>) -> NeonResult<Vec<(Vec<String>, SerializedValue)>> {
    let mut paths = vec![];
    let mut queue: VecDeque<(Vec<String>, Handle<JsObject>)> = VecDeque::new();
    queue.push_back((Vec::new(), d));

    while let Some((mut path, dict)) = queue.pop_front() {
        let keys = dict.get_own_property_names(cx)?;
        let len = keys.len(cx);

        for i in 0..len {
            let key = keys.get::<JsArray, _, u32>(cx, i).unwrap().downcast::<JsString, _>(cx).unwrap().value(cx);
            path.push(key.clone());

            let val: Handle<JsValue> = dict.get(cx, key.as_str())?;
            if val.is_a::<JsObject, _>(cx) {
                let sub_dict = val.downcast::<JsObject, _>(cx).unwrap();
                queue.push_back((path.clone(), sub_dict));
            } else {
                let v = from_js_value(cx, val)?;
                paths.push((path.clone(), v));
            }

            path.pop();
        }
    }

    Ok(paths)
}

struct NodeChidori {
    c: Arc<Mutex<Chidori>>
}

impl Finalize for NodeChidori {}

impl NodeChidori {

    fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<NodeChidori>> {
        let file_id = cx.argument::<JsString>(0)?.value(&mut cx);
        let url = cx.argument::<JsString>(1)?.value(&mut cx);

        if !url.contains("://") {
            return cx.throw_error("Invalid url, must include protocol");
        }
        // let api_token = cx.argument_opt(2)?.value(&mut cx);
        debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, "".to_string());
        Ok(cx.boxed(NodeChidori {
            c: Arc::new(Mutex::new(Chidori::new(file_id, url))),
        }))
    }

    fn start_server(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let (deferred, promise) = cx.promise();
        let file_path = cx.argument_opt(0).map(|x| x.downcast::<JsString, _>(&mut cx).unwrap().value(&mut cx));
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut c = c.lock().await;
            let m = c.start_server(file_path).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(_) = m {
                    Ok(cx.undefined())
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }

    fn play(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let branch = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let m = c.play(branch, frame).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)

    }

    fn pause(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let frame = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let m = c.pause(frame).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }

    fn branch(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let branch = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let m = c.branch(branch, frame).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }

    fn query(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let query = cx.argument::<JsString>(0).unwrap_or(JsString::new(&mut cx, "")).value(&mut cx);
        let branch = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(2).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;

        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let m = c.query(query, branch, frame).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }

    fn list_branches(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let m = c.list_branches().await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }

    fn display_graph_structure(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let branch = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let r= c.display_graph_structure(branch).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(r) = r {
                    Ok(cx.string(r))
                } else {
                    cx.throw_error("Error displaying graph structure")
                }
            });
        });
        Ok(promise)
    }

    fn list_registered_graphs(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let c = c.lock().await;
            let _ = c.list_registered_graphs().await;
            deferred.settle_with(&channel, move |mut cx| {
                Ok(cx.undefined())
            });
        });
        Ok(promise)
    }

//
//     // TODO: need to figure out how to handle callbacks
//     // fn list_input_proposals<'a>(
//     //     mut self_: PyRefMut<'_, Self>,
//     //     py: Python<'a>,
//     //     callback: PyObject
//     // ) -> PyResult<&'a PyAny> {
//     //     let file_id = self_.file_id.clone();
//     //     let url = self_.url.clone();
//     //     let branch = self_.current_branch;
//     //     pyo3_asyncio::tokio::future_into_py(py, async move {
//     //         let mut client = get_client(url).await?;
//     //         let resp = client.list_input_proposals(RequestOnlyId {
//     //             id: file_id,
//     //             branch,
//     //         }).await.map_err(PyErrWrapper::from)?;
//     //         let mut stream = resp.into_inner();
//     //         while let Some(x) = stream.next().await {
//     //             // callback.call(py, (x,), None);
//     //             info!("InputProposals = {:?}", x);
//     //         };
//     //         Ok(())
//     //     })
//     // }
//
//     // fn respond_to_input_proposal(mut self_: PyRefMut<'_, Self>) -> PyResult<()> {
//     //     Ok(())
//     // }
//
//     // TODO: need to figure out how to handle callbacks
//     // fn list_change_events<'a>(
//     //     mut self_: PyRefMut<'_, Self>,
//     //     py: Python<'a>,
//     //     callback: PyObject
//     // ) -> PyResult<&'a PyAny> {
//     //     let file_id = self_.file_id.clone();
//     //     let url = self_.url.clone();
//     //     let branch = self_.current_branch;
//     //     pyo3_asyncio::tokio::future_into_py(py, async move {
//     //         let mut client = get_client(url).await?;
//     //         let resp = client.list_change_events(RequestOnlyId {
//     //             id: file_id,
//     //             branch,
//     //         }).await.map_err(PyErrWrapper::from)?;
//     //         let mut stream = resp.into_inner();
//     //         while let Some(x) = stream.next().await {
//     //             Python::with_gil(|py| pyo3_asyncio::tokio::into_future(callback.as_ref(py).call((x.map(ChangeValueWithCounterWrapper).map_err(PyErrWrapper::from)?,), None)?))?
//     //                 .await?;
//     //         };
//     //         Ok(())
//     //     })
//     // }
//
//



    fn register_custom_node_handle(mut cx: FunctionContext) -> JsResult<JsValue> {
        let channel = cx.channel();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;

        let function_name: String = cx.argument::<JsString>(0)?.value(&mut cx);
        let callback = cx.argument::<JsFunction>(1)?.root(&mut cx);

        let h = callback.to_inner(&mut cx);
        let callback = Arc::new(callback);
        let c = Arc::clone(&self_.c);

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut c = c.lock().await;
            c.register_custom_node_handle(function_name, Handler::new(
                move |n| {
                    let channel_clone = channel.clone();
                    let handler_clone = Arc::clone(&callback);
                    Box::pin(async move {
                        // TODO: clean this up, can't use ?
                        let (tx, mut rx) = mpsc::channel::<serde_json::Value>(1);
                        if let Ok(_) = channel_clone.send(move |mut cx| {
                            if let Ok(v) = neon_serde3::to_value(&mut cx, &n) {
                                let js_function = JsFunction::new(&mut cx, move |mut cx| {
                                    if let Ok(v) = cx.argument::<JsValue>(0) {
                                        let value: Result<serde_json::Value, _> = neon_serde3::from_value(&mut cx, v);
                                        if let Ok(value) = value {
                                            tx.blocking_send(value).unwrap();
                                        }
                                    }
                                    Ok(cx.undefined())
                                })?;
                                let callback = handler_clone.to_inner(&mut cx);
                                let _: JsResult<JsValue> = callback.call_with(&mut cx).arg(v).arg(js_function).apply(&mut cx);
                            }
                            Ok(serde_json::Value::Null)
                        }).join() {
                            // block until we receive the result from the channel
                            if let Some(value) = rx.recv().await {
                                Ok(value)
                            } else {
                                Ok(serde_json::Value::Null)
                            }
                        } else {
                            Err(anyhow::anyhow!("Failed to send result"))
                        }
                    })
                }
            ));
        });
        Ok(cx.undefined().upcast())
    }


    fn run_custom_node_loop(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeChidori>, _>(&mut cx)?;
        let c = Arc::clone(&self_.c);
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut c = c.lock().await;
            let _ = c.run_custom_node_loop().await;
            deferred.settle_with((&channel), move |mut cx| {
                Ok(cx.undefined())
            });

        });
        // This promise is never resolved
        Ok(promise)
    }



}

struct NodeGraphBuilder {
    g: Arc<Mutex<GraphBuilder>>,
}

impl Finalize for NodeGraphBuilder {}

impl NodeGraphBuilder {
    fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<NodeGraphBuilder>> {
        Ok(cx.boxed(NodeGraphBuilder {
            g: Arc::new(Mutex::new(GraphBuilder::new())),
        }))
    }

//     // TODO: need to figure out passing a buffer of bytes
//     // TODO: nodes that are added should return a clean definition of what their addition looks like
//     // TODO: adding a node should also display any errors
//     /// x = None
//     /// with open("/Users/coltonpierson/Downloads/files_and_dirs.zip", "rb") as zip_file:
//     ///     contents = zip_file.read()
//     ///     x = await p.load_zip_file("LoadZip", """ output: String """, contents)
//     /// x
//     // #[pyo3(signature = (name=String::new(), output_tables=vec![], output=String::new(), bytes=vec![]))]
//     // fn load_zip_file<'a>(
//     //     mut self_: PyRefMut<'_, Self>,
//     //     py: Python<'a>,
//     //     name: String,
//     //     output_tables: Vec<String>,
//     //     output: String,
//     //     bytes: Vec<u8>
//     // ) -> PyResult<&'a PyAny> {
//     //     let file_id = self_.file_id.clone();
//     //     let url = self_.url.clone();
//     //     pyo3_asyncio::tokio::future_into_py(py, async move {
//     //         let node = create_loader_node(
//     //             name,
//     //             vec![],
//     //             output,
//     //             LoadFrom::ZipfileBytes(bytes),
//     //             output_tables
//     //         );
//     //         Ok(push_file_merge(&url, &file_id, node).await?)
//     //     })
//     // }
//
//     // TODO: this should accept an "Object" instead of args
//     // TODO: nodes that are added should return a clean definition of what their addition looks like
//     // TODO: adding a node should also display any errors


    fn prompt_node(mut cx: FunctionContext) -> JsResult<JsBox<RefCell<NodeNodeHandle>>> {
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeGraphBuilder>, _>(&mut cx)?;
        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: PromptNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };
        let mut g = self_.g.blocking_lock();
        match g.prompt_node(arg0_value) {
            Ok(result) => Ok(cx.boxed(RefCell::new(NodeNodeHandle::from(result)))),
            Err(e) => cx.throw_error(e.to_string())
        }
    }


    fn custom_node(mut cx: FunctionContext) -> JsResult<JsBox<RefCell<NodeNodeHandle>>> {
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeGraphBuilder>, _>(&mut cx)?;
        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: CustomNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };
        let mut g = self_.g.blocking_lock();
        match g.custom_node(arg0_value) {
            Ok(result) => Ok(cx.boxed(RefCell::new(NodeNodeHandle::from(result)))),
            Err(e) => cx.throw_error(e.to_string())
        }
    }

    fn deno_code_node(mut cx: FunctionContext) -> JsResult<JsBox<RefCell<NodeNodeHandle>>> {
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeGraphBuilder>, _>(&mut cx)?;
        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: DenoCodeNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };
        let mut g = self_.g.blocking_lock();
        match g.deno_code_node(arg0_value) {
            Ok(result) => Ok(cx.boxed(RefCell::new(NodeNodeHandle::from(result)))),
            Err(e) => cx.throw_error(e.to_string())
        }
    }

    fn vector_memory_node(mut cx: FunctionContext) -> JsResult<JsBox<RefCell<NodeNodeHandle>>> {
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeGraphBuilder>, _>(&mut cx)?;
        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: VectorMemoryNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };
        let mut g = self_.g.blocking_lock();
        match g.vector_memory_node(arg0_value) {
            Ok(result) => Ok(cx.boxed(RefCell::new(NodeNodeHandle::from(result)))),
            Err(e) => cx.throw_error(e.to_string())
        }
    }



    fn commit(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<NodeGraphBuilder>, _>(&mut cx)?;
        let node_chidori = cx.argument::<JsBox<NodeChidori>>(0)?;
        let branch = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;

        let c = Arc::clone(&node_chidori.c);
        let g = Arc::clone(&self_.g);

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut graph_builder = g.lock().await;
            let mut chidori = c.lock().await;
            let m = graph_builder.commit(&mut chidori, branch).await;
            deferred.settle_with((&channel), move |mut cx| {
                if let Ok(result) = m {
                    neon_serde3::to_value(&mut cx, &result)
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Error playing")
                }
            });
        });
        Ok(promise)
    }
//
//
//     //
//     // fn observation_node(mut self_: PyRefMut<'_, Self>, name: String, query_def: Option<String>, template: String, model: String) -> PyResult<()> {
//     //     let file_id = self_.file_id.clone();
//     //     let node = create_observation_node(
//     //         "".to_string(),
//     //         None,
//     //         "".to_string(),
//     //     );
//     //     executor::block_on(self_.client.merge(RequestFileMerge {
//     //         id: file_id,
//     //         file: Some(File {
//     //             nodes: vec![node],
//     //             ..Default::default()
//     //         }),
//     //         branch: 0,
//     //     }));
//     //     Ok(())
//     // }
}


fn neon_simple_fun(mut cx: FunctionContext) -> JsResult<JsString> {
    let port = cx.argument::<JsString>(0)?.value(&mut cx);
    Ok(cx.string(port))
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    env_logger::init();
    cx.export_function("nodehandleRunWhen", NodeNodeHandle::run_when)?;
    // cx.export_function("nodehandleQuery", NodeNodeHandle::query)?;
    cx.export_function("chidoriNew", NodeChidori::js_new)?;
    cx.export_function("chidoriStartServer", NodeChidori::start_server)?;
    cx.export_function("chidoriPlay", NodeChidori::play)?;
    cx.export_function("chidoriPause", NodeChidori::pause)?;
    cx.export_function("chidoriBranch", NodeChidori::branch)?;
    cx.export_function("chidoriQuery", NodeChidori::query)?;
    cx.export_function("chidoriGraphStructure", NodeChidori::display_graph_structure)?;
    cx.export_function("chidoriRegisterCustomNodeHandle", NodeChidori::register_custom_node_handle)?;
    cx.export_function("chidoriRunCustomNodeLoop", NodeChidori::run_custom_node_loop)?;

    cx.export_function("graphbuilderNew", NodeGraphBuilder::js_new)?;
    cx.export_function("graphbuilderCustomNode", NodeGraphBuilder::custom_node)?;
    cx.export_function("graphbuilderDenoCodeNode", NodeGraphBuilder::deno_code_node)?;
    cx.export_function("graphbuilderPromptNode", NodeGraphBuilder::prompt_node)?;
    cx.export_function("graphbuilderVectorMemoryNode", NodeGraphBuilder::vector_memory_node)?;
    cx.export_function("graphbuilderCommit", NodeGraphBuilder::commit)?;
    cx.export_function("simpleFun", neon_simple_fun)?;
    Ok(())
}