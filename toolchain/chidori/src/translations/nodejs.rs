use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use anyhow::Error;
use futures::StreamExt;
use log::{debug, info};
use neon::{prelude::*, types::Deferred};
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
use serde::{Deserialize, Serialize};

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


async fn push_file_merge(url: &String, file_id: &String, node: Item) -> anyhow::Result<NodeHandle> {
    let mut client = get_client(url.clone()).await?;
    let exec_status = client.merge(RequestFileMerge {
        id: file_id.clone(),
        file: Some(File {
            nodes: vec![node.clone()],
            ..Default::default()
        }),
        branch: 0,
    }).await?.into_inner();
    Ok(NodeHandle::from(
        url.clone(),
        file_id.clone(),
        node,
        exec_status
    )?)
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


// Node handle
#[derive(Clone)]
pub struct NodeHandle {
    url: String,
    file_id: String,
    node: Item,
    exec_status: ExecutionStatus,
    indiv: CleanIndividualNode
}

impl NodeHandle {
    fn example() -> Self{
        let node = create_code_node(
            "Example".to_string(),
            vec![None],
            "type O { output: String }".to_string(),
            SourceNodeType::Code("DENO".to_string(), r#"return {"output": "hello"}"#.to_string(), false),
            vec![],
        );
        let indiv = derive_for_individual_node(&node).unwrap();
        NodeHandle {
            url: "localhost:9800".to_string(),
            file_id: "0".to_string(),
            node: node,
            exec_status: Default::default(),
            indiv,
        }
    }

    fn from(url: String, file_id: String, node: Item, exec_status: ExecutionStatus) -> anyhow::Result<NodeHandle> {
        let indiv = derive_for_individual_node(&node)?;
        Ok(NodeHandle {
            url,
            file_id,
            node,
            exec_status,
            indiv
        })
    }
}

impl Finalize for NodeHandle {}


impl NodeHandle {
    pub fn js_debug_example(mut cx: FunctionContext) -> JsResult<JsBox<RefCell<NodeHandle>>> {
        let nh = NodeHandle::example();
        Ok(cx.boxed(RefCell::new(nh)))
    }

    fn get_name(&self) -> String {
        self.node.core.as_ref().unwrap().name.clone()
    }

    pub fn run_when(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let other_node = cx.argument::<JsBox<RefCell<NodeHandle>>>(0)?.downcast_or_throw::<JsBox<RefCell<NodeHandle>>, _>(&mut cx)?;

        let self_ = cx.this()
            .downcast_or_throw::<JsBox<RefCell<NodeHandle>>, _>(&mut cx)?;

        let mut self_borrow = self_.borrow_mut();
        let queries = &mut self_borrow.node.core.as_mut().unwrap().queries;

        // Get the constructed query from the target node
        let q = construct_query_from_output_type(
            &other_node.borrow().get_name(),
            &other_node.borrow().get_name(),
            &self_.borrow().indiv.output_path
        ).unwrap();

        queries.push(Query { query: Some(q)});

        let url = self_.borrow().url.clone();
        let file_id = self_.borrow().file_id.clone();
        let node = self_.borrow().node.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let result = push_file_merge(&url, &file_id, node).await.unwrap();
            deferred.settle_with(&channel, move |mut cx| {
                Ok(cx.boolean(true))
            });
        });
        Ok(promise)
    }


    pub fn query(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let (deferred, promise) = cx.promise();
        let channel = cx.channel();

        let branch = cx.argument::<JsNumber>(0)?.value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(1)?.value(&mut cx) as u64;

        let self_ = cx.this()
            .downcast_or_throw::<JsBox<RefCell<NodeHandle>>, _>(&mut cx)?;
        let mut self_borrow = self_.borrow();
        let file_id = self_borrow.file_id.clone();
        let url = self_borrow.url.clone();
        let name = &self_borrow.node.core.as_ref().unwrap().name;

        let query = construct_query_from_output_type(&name, &name, &self_.borrow().indiv.output_path).unwrap();

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.run_query(QueryAtFrame {
                id: file_id,
                query: Some(Query {
                    query: Some(query)
                }),
                frame,
                branch,
            }).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    let res = result.into_inner();
                    let mut obj = cx.empty_object();
                    for value in res.values.iter() {
                        let c = value.change_value.as_ref().unwrap();
                        let k = c.path.as_ref().unwrap().address.join(":");
                        let v = c.value.as_ref().unwrap().clone();
                        let js = SerializedValueWrapper(v).to_object(&mut cx);
                        obj.set(&mut cx, k.as_str(), js?).unwrap();
                    }
                    Ok(obj)
                } else {
                    cx.throw_error("Failed to query")
                }
            });
        });
        Ok(promise)
    }


    // fn js_to_string(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    //     let branch = cx.argument::<JsString>(0)?.value(&mut cx);
    //     let frame = cx.argument::<JsString>(1)?.value(&mut cx);
    //
    //     let channel = cx.channel();
    //
    //     // let name = self.get_name();
    //     Ok(format!("NodeHandle(file_id={}, node={})", self.file_id, name))
    //     //
    //     // Ok(cx.undefined())
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




#[derive(serde::Serialize, serde::Deserialize)]
struct PromptNodeCreateOpts {
    name: String,
    queries: Option<Vec<String>>,
    output_tables: Option<Vec<String>>,
    template: String,
    model: Option<String>
}


#[derive(serde::Serialize, serde::Deserialize)]
struct CustomNodeCreateOpts {
    name: String,
    queries: Option<Vec<String>>,
    output_tables: Option<Vec<String>>,
    output: Option<String>,
    node_type_name: String
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DenoCodeNodeCreateOpts {
    name: String,
    queries: Option<Vec<String>>,
    output_tables: Option<Vec<String>>,
    output: Option<String>,
    code: String,
    is_template: Option<bool>
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VectorMemoryNodeCreateOpts {
    name: String,
    queries: Option<Vec<String>>,
    output_tables: Option<Vec<String>>,
    output: Option<String>,
    template: Option<String>, // TODO: default is the contents of the query
    action: Option<String>, // TODO: default WRITE
    embedding_model: Option<String>, // TODO: default TEXT_EMBEDDING_ADA_002
    db_vendor: Option<String>, // TODO: default QDRANT
    collection_name: String,
}


struct Chidori {
    file_id: String,
    current_head: u64,
    current_branch: u64,
    url: String
}

impl Finalize for Chidori {}

impl Chidori {

    fn js_new(mut cx: FunctionContext) -> JsResult<JsBox<Chidori>> {
        let file_id = cx.argument::<JsString>(0)?.value(&mut cx);
        let url = cx.argument::<JsString>(1)?.value(&mut cx);

        if !url.contains("://") {
            return cx.throw_error("Invalid url, must include protocol");
        }
        // let api_token = cx.argument_opt(2)?.value(&mut cx);
        debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, "".to_string());
        Ok(cx.boxed(Chidori {
            file_id,
            current_head: 0,
            current_branch: 0,
            url,
        }))
    }

    fn start_server(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let (deferred, promise) = cx.promise();
        let url_server = self_.url.clone();
        let file_path: Option<String> = match cx.argument_opt(0) {
            Some(v) => Some(v.downcast_or_throw(&mut cx)),
            None => None,
        }.map(|p: JsResult<JsString>| p.unwrap().value(&mut cx));
        std::thread::spawn(move || {
            let result = run_server(url_server, file_path);
            match result {
                Ok(_) => {
                    println!("Server exited");
                },
                Err(e) => {
                    println!("Error running server: {}", e);
                },
            }
        });

        let url = self_.url.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            'retry: loop {
                let client = get_client(url.clone());
                match client.await {
                    Ok(connection) => {
                        eprintln!("Connection successfully established {:?}", &url);
                        deferred.settle_with(&channel, move |mut cx| {
                            Ok(cx.undefined())
                        });
                        break 'retry
                    },
                    Err(e) => {
                        eprintln!("Error connecting to server: {} with Error {}. Retrying...", &url, &e.to_string());
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                    }
                }
            }
        });
        Ok(promise)
    }

    fn play(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let branch = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.play(RequestAtFrame {
                id: file_id,
                frame,
                branch,
            }).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Failed to play runtime.")
                }
            });
        });
        Ok(promise)
    }

    fn pause(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let frame = cx.argument::<JsNumber>(0).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.pause(RequestAtFrame {
                id: file_id,
                frame,
                branch,
            }).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Failed to play runtime.")
                }
            });
        });
        Ok(promise)
    }

    fn branch(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.branch(RequestNewBranch {
                id: file_id,
                source_branch_id: branch,
                diverges_at_counter: 0,
            }).await;
            // TODO: need to somehow handle writing to the current_branch
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Failed to play runtime.")
                }
            });
        });
        Ok(promise)
    }

    fn query(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let query = cx.argument::<JsString>(0).unwrap_or(JsString::new(&mut cx, "")).value(&mut cx);
        let branch = cx.argument::<JsNumber>(1).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let frame = cx.argument::<JsNumber>(2).unwrap_or(JsNumber::new(&mut cx, 0.0)).value(&mut cx) as u64;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.run_query(QueryAtFrame {
                id: file_id,
                query: Some(Query {
                    query: Some(query)
                }),
                frame,
                branch,
            }).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Failed to play runtime.")
                }
            });
        });
        Ok(promise)
    }

    fn list_branches(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let result = client.list_branches(RequestListBranches {
                id: file_id,
            }).await;
            deferred.settle_with(&channel, move |mut cx| {
                if let Ok(result) = result {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                } else {
                    cx.throw_error("Failed to play runtime.")
                }
            });
        });
        Ok(promise)
    }

    fn display_graph_structure(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let file = if let Ok(file) = client.current_file_state(RequestOnlyId {
                id: file_id,
                branch
            }).await {
                file
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to get current file state.");
                    Ok(cx.undefined())
                });
                panic!("Failed to get current file state.");
            };
            let mut file = file.into_inner();
            let mut g = CleanedDefinitionGraph::zero();
            g.merge_file(&mut file).unwrap();
            deferred.settle_with(&channel, move |mut cx| {
                Ok(cx.string(g.get_dot_graph()))
            });
        });
        Ok(promise)
    }

//
//     // TODO: some of these register handlers instead
//     // TODO: list registered graphs should not stream
//     // TODO: add a message that sends the current graph state
//

    fn list_registered_graphs(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            let resp = if let Ok(resp) = client.list_registered_graphs(Empty {
            }).await {
                resp
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to get registered graph stream.");
                    Ok(cx.undefined())
                });
                panic!("Failed to get registered graph stream.");
            };
            let mut stream = resp.into_inner();
            while let Some(x) = stream.next().await {
                // callback.call(py, (x,), None);
                info!("Registered Graph = {:?}", x);
            };
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


    fn prompt_node(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;

        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: PromptNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };

        let queries: Vec<Option<String>> = if let Some(queries) = arg0_value.queries {
            queries.into_iter().map(|q| {
                if q == "None".to_string() {
                    None
                } else {
                    Some(q)
                }
            }).collect()
        } else {
            vec![None]
        };

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let prompt_node = create_prompt_node(
                arg0_value.name,
                queries,
                arg0_value.template,
                arg0_value.model.unwrap_or("GPT_3_5_TURBO".to_string()),
                arg0_value.output_tables.unwrap_or(vec![]));
            let node = if let Ok(node) = prompt_node {
                node
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };

            if let Ok(result ) = push_file_merge(&url, &file_id, node).await {
                deferred.settle_with(&channel, move |mut cx| {
                    Ok(cx.boxed(RefCell::new(result)))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
        });
        Ok(promise)
    }


    fn poll_local_code_node_execution(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            if let Ok(result) = client.poll_node_will_execute_events(FilteredPollNodeWillExecuteEventsRequest {
                id: file_id.clone(),
            }).await {
                debug!("poll_local_code_node_execution result = {:?}", result);
                deferred.settle_with(&channel, move |mut cx| {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
            };
        });
        Ok(promise)
    }
    fn ack_local_code_node_execution(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = cx.argument::<JsNumber>(0)?.value(&mut cx) as u64;
        let counter = cx.argument::<JsNumber>(1)?.value(&mut cx) as u64;
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
            if let Ok(result) = client.ack_node_will_execute_event(RequestAckNodeWillExecuteEvent {
                id: file_id.clone(),
                branch,
                counter,
            }).await {
                deferred.settle_with(&channel, move |mut cx| {
                    neon_serde3::to_value(&mut cx, &result.into_inner())
                        .or_else(|e| cx.throw_error(e.to_string()))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
            }
        });
        Ok(promise)
    }

    fn respond_local_code_node_execution(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();

        let branch = cx.argument::<JsNumber>(0)?.value(&mut cx) as u64;
        let counter = cx.argument::<JsNumber>(1)?.value(&mut cx) as u64;
        let node_name = cx.argument::<JsString>(2)?.value(&mut cx);

        let response: Option<JsResult<JsObject>> = match cx.argument_opt(0) {
            Some(v) => Some(v.downcast_or_throw(&mut cx)),
            None => None,
        };

        // TODO: need parent counters from the original change
        // TODO: need source node

        let response_paths = if let Some(response) = response {
            // TODO: need better error handling here
            obj_to_paths(&mut cx, response.unwrap()).unwrap()
        } else {
            vec![]
        };

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let mut client = if let Ok(mut client) = get_client(url).await {
                client
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    cx.throw_error::<&str, JsUndefined>("Failed to connect to runtime service.");
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };

            // TODO: need to add the output table paths to these
            let filled_values = response_paths.into_iter().map(|path| {
                ChangeValue {
                    path: Some(Path {
                        address: path.0,
                    }),
                    value: Some(path.1),
                    branch,
                }
            });

            // TODO: this needs to look more like a real change
            client.push_worker_event(FileAddressedChangeValueWithCounter {
                branch,
                counter,
                node_name,
                id: file_id.clone(),
                change: Some(ChangeValueWithCounter {
                    filled_values: filled_values.collect(),
                    parent_monotonic_counters: vec![],
                    monotonic_counter: counter,
                    branch,
                    source_node: "".to_string(),
                })
            }).await.unwrap();
        });
        Ok(promise)
    }

//     // }
//
//     // TODO: handle dispatch to this handler - should accept a callback
//     // https://github.com/PyO3/pyo3/issues/525
    fn custom_node(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;

        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: CustomNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };

        let queries: Vec<Option<String>> = if let Some(queries) = arg0_value.queries {
            queries.into_iter().map(|q| {
                if q == "None".to_string() {
                    None
                } else {
                    Some(q)
                }
            }).collect()
        } else {
            vec![]
        };

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            // Register the node with the system
            let node = create_custom_node(
                arg0_value.name,
                queries,
                arg0_value.output.unwrap_or("type O {}".to_string()),
                arg0_value.node_type_name,
                arg0_value.output_tables.unwrap_or(vec![])
            );
            if let Ok(result ) = push_file_merge(&url, &file_id, node).await {
                deferred.settle_with(&channel, move |mut cx| {
                    Ok(cx.boxed(RefCell::new(result)))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
        });
        Ok(promise)
    }

    fn deno_code_node(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;

        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: DenoCodeNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };

        let queries: Vec<Option<String>> = if let Some(queries) = arg0_value.queries {
            queries.into_iter().map(|q| {
                if q == "None".to_string() {
                    None
                } else {
                    Some(q)
                }
            }).collect()
        } else {
            vec![None]
        };

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;

        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let node = create_code_node(
                arg0_value.name,
                queries,
                arg0_value.output.unwrap_or("type O {}".to_string()),
                SourceNodeType::Code("DENO".to_string(), arg0_value.code, arg0_value.is_template.unwrap_or(false)),
                arg0_value.output_tables.unwrap_or(vec![])
            );
            if let Ok(result ) = push_file_merge(&url, &file_id, node).await {
                deferred.settle_with(&channel, move |mut cx| {
                    Ok(cx.boxed(RefCell::new(result)))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
        });
        Ok(promise)
    }

    fn vector_memory_node(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let channel = cx.channel();
        let (deferred, promise) = cx.promise();
        let self_ = cx.this()
            .downcast_or_throw::<JsBox<Chidori>, _>(&mut cx)?;

        let arg0 = cx.argument::<JsValue>(0)?;
        let arg0_value: VectorMemoryNodeCreateOpts = match neon_serde3::from_value(&mut cx, arg0) {
            Ok(value) => value,
            Err(e) => {
                return cx.throw_error(e.to_string());
            }
        };

        let queries: Vec<Option<String>> = if let Some(queries) = arg0_value.queries {
            queries.into_iter().map(|q| {
                if q == "None".to_string() {
                    None
                } else {
                    Some(q)
                }
            }).collect()
        } else {
            vec![]
        };

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let node = create_vector_memory_node(
                arg0_value.name,
                queries,
                arg0_value.output.unwrap_or("type O {}".to_string()),
                arg0_value.action.unwrap_or("READ".to_string()),
                arg0_value.embedding_model.unwrap_or("TEXT_EMBEDDING_ADA_002".to_string()),
                arg0_value.template.unwrap_or("".to_string()),
                arg0_value.db_vendor.unwrap_or("QDRANT".to_string()),
                arg0_value.collection_name,
                arg0_value.output_tables.unwrap_or(vec![])
            );
            let node = if let Ok(node) = node {
                node
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };

            if let Ok(result ) = push_file_merge(&url, &file_id, node).await {
                deferred.settle_with(&channel, move |mut cx| {
                    Ok(cx.boxed(RefCell::new(result)))
                });
            } else {
                deferred.settle_with(&channel, move |mut cx| {
                    // TODO: throw error
                    Ok(cx.undefined())
                });
                panic!("Failed to connect to runtime service.");
            };
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
    cx.export_function("nodehandleDebugExample", NodeHandle::js_debug_example)?;
    cx.export_function("nodehandleRunWhen", NodeHandle::run_when)?;
    cx.export_function("nodehandleQuery", NodeHandle::query)?;
    cx.export_function("chidoriNew", Chidori::js_new)?;
    cx.export_function("chidoriStartServer", Chidori::start_server)?;
    cx.export_function("chidoriPlay", Chidori::play)?;
    cx.export_function("chidoriPause", Chidori::pause)?;
    cx.export_function("chidoriBranch", Chidori::branch)?;
    cx.export_function("chidoriQuery", Chidori::query)?;
    cx.export_function("chidoriGraphStructure", Chidori::display_graph_structure)?;
    cx.export_function("chidoriObjInterface", Chidori::obj_interface)?;
    cx.export_function("chidoriCustomNode", Chidori::custom_node)?;
    cx.export_function("chidoriDenoCodeNode", Chidori::deno_code_node)?;
    cx.export_function("chidoriVectorMemoryNode", Chidori::vector_memory_node)?;
    cx.export_function("chidoriPollLocalCodeNodeExecution", Chidori::poll_local_code_node_execution)?;
    cx.export_function("chidoriAckLocalCodeNodeExecution", Chidori::ack_local_code_node_execution)?;
    cx.export_function("chidoriRespondLocalCodeNodeExecution", Chidori::respond_local_code_node_execution)?;
    cx.export_function("simpleFun", neon_simple_fun)?;
    Ok(())
}