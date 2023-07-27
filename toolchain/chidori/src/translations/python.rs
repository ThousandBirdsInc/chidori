use std::collections::VecDeque;
use pyo3::exceptions;
use pyo3::prelude::*;
use tonic::{Response, Status};
use futures::executor;
use futures::StreamExt;
use pyo3::types::{PyDict, PyList, PyString};
use pyo3::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc};
use tokio::sync::Mutex;
use std::time::Duration;
use tokio::runtime::Runtime;
use log::{debug, info};
use pyo3::exceptions::PyTypeError;
use tonic::transport::Channel;
use ::prompt_graph_exec::tonic_runtime::run_server;
use ::prompt_graph_core::proto2::execution_runtime_client::ExecutionRuntimeClient;
use ::prompt_graph_core::proto2::{File, Empty, ExecutionStatus, RequestInputProposalResponse, RequestOnlyId, RequestFileMerge, RequestAtFrame, RequestNewBranch, RequestListBranches, ListBranchesRes, QueryAtFrame, Query, QueryAtFrameResponse, SerializedValue, ChangeValueWithCounter, NodeWillExecuteOnBranch, FileAddressedChangeValueWithCounter, FilteredPollNodeWillExecuteEventsRequest, RespondPollNodeWillExecuteEvents, RequestAckNodeWillExecuteEvent, ChangeValue, Path, SerializedValueObject, SerializedValueArray, Item};
use ::prompt_graph_core::proto2::prompt_graph_node_loader::LoadFrom;
use ::prompt_graph_core::proto2::serialized_value::Val;
use ::prompt_graph_core::graph_definition::{create_prompt_node, create_op_map, create_code_node, create_component_node, create_vector_memory_node, create_observation_node, create_node_parameter, SourceNodeType, create_loader_node, create_custom_node};
use ::prompt_graph_core::utils::wasm_error::CoreError;
use ::prompt_graph_core::build_runtime_graph::graph_parse::{CleanedDefinitionGraph, CleanIndividualNode, construct_query_from_output_type, derive_for_individual_node};
use crate::register_node_handle;
use crate::translations::rust::{Chidori, CustomNodeCreateOpts, DenoCodeNodeCreateOpts, GraphBuilder, Handler, NodeHandle, PromptNodeCreateOpts, VectorMemoryNodeCreateOpts};

#[derive(Debug)]
pub struct CoreErrorWrapper(CoreError);

impl std::convert::From<CoreErrorWrapper> for PyErr {
    fn from(err: CoreErrorWrapper) -> PyErr {
        exceptions::PyOSError::new_err(err.0.to_string())
    }
}

impl std::convert::From<CoreError> for PyErrWrapper {
    fn from(err: CoreError) -> PyErrWrapper {
        PyErrWrapper(exceptions::PyOSError::new_err(err.to_string()))
    }
}

impl std::convert::From<anyhow::Error> for PyErrWrapper {
    fn from(err: anyhow::Error) -> PyErrWrapper {
        PyErrWrapper(exceptions::PyOSError::new_err(err.to_string()))
    }
}


#[derive(Debug)]
pub struct PyErrWrapper(pyo3::PyErr);

#[derive(Debug)]
pub struct AnyhowErrWrapper(anyhow::Error);


impl std::convert::From<tonic::transport::Error> for PyErrWrapper {
    fn from(status: tonic::transport::Error) -> Self {
        // Convert the `Status` to a `PyErr` here, then wrap it in `PyErrWrapper`.
        PyErrWrapper(exceptions::PyOSError::new_err(status.to_string()))
    }
}

impl std::convert::From<Status> for PyErrWrapper {
    fn from(status: Status) -> Self {
        // Convert the `Status` to a `PyErr` here, then wrap it in `PyErrWrapper`.
        PyErrWrapper(exceptions::PyOSError::new_err(status.message().to_string()))
    }
}


impl std::convert::From<PyErrWrapper> for PyErr {
    fn from(err: PyErrWrapper) -> PyErr {
        err.0
    }
}

impl std::convert::From<AnyhowErrWrapper> for PyErr {
    fn from(err: AnyhowErrWrapper) -> PyErr {
        exceptions::PyOSError::new_err(err.0.to_string())
    }
}

impl Into<pyo3::PyResult<()>> for PyErrWrapper {
    fn into(self) -> pyo3::PyResult<()> {
        Err(self.0)
    }
}


pub struct PyExecutionStatus(ExecutionStatus);


impl IntoPy<Py<PyAny>> for PyExecutionStatus {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let exec_status = self.0;
        let dict = PyDict::new(py);
        dict.set_item("id", exec_status.id).unwrap();
        dict.set_item("monotonic_counter", exec_status.monotonic_counter).unwrap();
        dict.set_item("branch", exec_status.branch).unwrap();
        dict.into_py(py)
    }
}

pub struct PyResponseExecutionStatus(Response<ExecutionStatus>);


impl IntoPy<Py<PyAny>> for PyResponseExecutionStatus {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let PyResponseExecutionStatus(resp) = self;
        let exec_status = resp.into_inner();
        let dict = PyDict::new(py);
        dict.set_item("id", exec_status.id).unwrap();
        dict.set_item("monotonic_counter", exec_status.monotonic_counter).unwrap();
        dict.set_item("branch", exec_status.branch).unwrap();
        dict.into_py(py)
    }
}


pub struct PyListBranchesRes(Response<ListBranchesRes>);


impl IntoPy<Py<PyAny>> for PyListBranchesRes {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let PyListBranchesRes(resp) = self;
        let branches = resp.into_inner();
        let branch_list = branches.branches.into_iter().map(|branch| {
            let mut dict = PyDict::new(py);
            dict.set_item("id", branch.id).unwrap();
            dict.set_item("diverges_at_counter", branch.diverges_at_counter).unwrap();
            dict.set_item("source_branch_ids", format!("{:?}", branch.source_branch_ids)).unwrap();
            dict
        }).collect::<Vec<_>>();
        PyList::new(py, branch_list).into_py(py)
    }
}


pub struct PyQueryAtFrameResponse(Response<QueryAtFrameResponse>);

impl IntoPy<Py<PyAny>> for PyQueryAtFrameResponse {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let PyQueryAtFrameResponse(resp) = self;
        let res = resp.into_inner();

        let mut dict = PyDict::new(py);
        res.values.into_iter().for_each(|value| {
            let c = value.change_value.unwrap();
            let k = c.path.unwrap().address.join(":");
            let v = c.value.unwrap();
            // TODO: SerializeValue to python
            dict.set_item(k, SerializedValueWrapper(v)).unwrap();
        });
        PyList::new(py, dict).into_py(py)
    }
}

#[derive(Debug)]
pub struct SerializedValueWrapper(SerializedValue);

impl ToPyObject for SerializedValueWrapper {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        if let None = self.0.val {
            let x: Option<bool> = None;
            return x.into_py(py);
        }
        match self.0.val.as_ref().unwrap() {
            Val::Float(x) => { x.into_py(py) }
            Val::Number(x) => { x.into_py(py) }
            Val::String(x) => { x.into_py(py) }
            Val::Boolean(x) => { x.into_py(py) }
            Val::Array(val) => {
                let py_list = PyList::empty(py);
                for item in &val.values {
                    py_list.append(SerializedValueWrapper(item.clone()).to_object(py)).unwrap();
                }
                py_list.into_py(py)
            }
            Val::Object(val) => {
                let py_dict = PyDict::new(py);
                for (key, value) in &val.values {
                    py_dict.set_item(key, SerializedValueWrapper(value.clone()).to_object(py)).unwrap();
                }
                py_dict.into_py(py)
            }
        }
    }
}

#[derive(Debug)]
pub struct ChangeValueWithCounterWrapper(ChangeValueWithCounter);

impl IntoPy<Py<PyAny>> for ChangeValueWithCounterWrapper {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let mut dict = PyDict::new(py);
        self.0.filled_values.into_iter().for_each(|c| {
            let k = c.path.map(|path| path.address.join(":"));
            let v = c.value;
            if let (Some(k), Some(v)) = (k, v) {
                dict.set_item(k, SerializedValueWrapper(v)).unwrap();
            }
        });
        PyList::new(py, dict).into_py(py)
    }
}

fn add_to_dict<'p>(
    py: Python<'p>,
    keys: &Vec<String>,
    value: PyObject,
    d: &'p PyDict,
) -> PyResult<()> {
    if keys.len() == 1 {
        d.set_item(keys[0].clone(), value)?;
    } else {
        let key = &keys[0];
        if d.contains(key)? {
            let nested_dict = d.get_item(key).unwrap().downcast::<PyDict>()?;
            add_to_dict(py, &keys[1..].to_vec(), value, &nested_dict)?;
        } else {
            let nested_dict = PyDict::new(py);
            d.set_item(key, &nested_dict)?;
            add_to_dict(py, &keys[1..].to_vec(), value, &nested_dict)?;
        }
    }
    Ok(())
}


fn pyany_to_serialized_value(p: &PyAny) -> SerializedValue {
    match p.get_type().name() {
        Ok(s) => {
            match s {
                "int" => {
                    let val = p.extract::<i32>().unwrap();
                    SerializedValue {
                        val: Some(Val::Number(val)),
                    }
                }
                "float" => {
                    let val = p.extract::<f32>().unwrap();
                    SerializedValue {
                        //         file: Some(File {
                        val: Some(Val::Float(val)),
                    }
                }
                "str" => {
                    let val = p.extract::<String>().unwrap();
                    SerializedValue {
                        val: Some(Val::String(val)),
                    }
                }
                "bool" => {
                    let val = p.extract::<bool>().unwrap();
                    SerializedValue {
                        val: Some(Val::Boolean(val)),
                    }
                }
                "list" => {
                    let list = p.downcast::<PyList>().unwrap();
                    let arr = list
                        .iter()
                        .map(|item| pyany_to_serialized_value(item))
                        .collect();
                    SerializedValue {
                        val: Some(Val::Array(SerializedValueArray { values: arr } )),
                    }
                }
                "dict" => {
                    let dict = p.downcast::<PyDict>().unwrap();
                    let mut map = HashMap::new();
                    for (key, value) in dict {
                        let key_string = key.extract::<String>().unwrap();
                        map.insert(key_string, pyany_to_serialized_value(value));
                    }
                    SerializedValue {
                        val: Some(Val::Object(SerializedValueObject { values: map })),
                    }
                }
                _ => SerializedValue::default(),
            }
        }
        Err(_) => SerializedValue::default(),
    }
}


use pyo3::prelude::*;
use pyo3::types::IntoPyDict;
use serde_json::json;

fn py_to_json<'p>(py: Python<'p>, v: &PyAny) -> serde_json::Value {
    if v.is_none() {
        json!(null)
    } else if let Ok(b) = v.extract::<bool>() {
        json!(b)
    } else if let Ok(i) = v.extract::<i64>() {
        json!(i)
    } else if let Ok(f) = v.extract::<f64>() {
        json!(f)
    } else if let Ok(dict) = v.extract::<HashMap<String, Py<PyAny>>>() {
        let mut m = serde_json::map::Map::new();
        for (key, value) in dict {
            m.insert(key, py_to_json(py, value.as_ref(py)));
        }
        json!(m)
    } else if let Ok(list) = v.extract::<Vec<Py<PyAny>>>() {
        let v: Vec<serde_json::Value> = list.iter().map(|p| py_to_json(py, p.as_ref(py))).collect();
        json!(v)
    } else if let Ok(s) = v.extract::<String>() {
        json!(s)
    } else {
        json!(null)
    }
}



fn dict_to_paths<'p>(
    py: Python<'p>,
    d: &'p PyDict
) -> PyResult<Vec<(Vec<String>, SerializedValue)>> {
    let mut paths = Vec::new();
    let mut queue: VecDeque<(Vec<String>, &'p PyDict)> = VecDeque::new();
    queue.push_back((Vec::new(), d));

    while let Some((mut path, dict)) = queue.pop_front() {
        for (key, val) in dict {
            let key_str = key.extract::<String>()?;
            path.push(key_str.clone());
            match val.downcast::<PyDict>() {
                Ok(sub_dict) => {
                    queue.push_back((path.clone(), sub_dict));
                },
                Err(_) => {
                    paths.push((path.clone(), pyany_to_serialized_value(val)));
                }
            }
            path.pop();
        }
    }

    Ok(paths)
}


#[derive(Debug)]
pub struct NodeWillExecuteOnBranchWrapper(NodeWillExecuteOnBranch);

impl ToPyObject for NodeWillExecuteOnBranchWrapper {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        let NodeWillExecuteOnBranch { branch, counter, node, custom_node_type_name} = &self.0;
        let dict = PyDict::new(py);
        dict.set_item("branch", branch).unwrap();
        dict.set_item("counter", counter).unwrap();
        if let Some(node) = node {
            dict.set_item("node_name", &node.source_node).unwrap();
            dict.set_item("type_name", &custom_node_type_name).unwrap();

            let event_dict = PyDict::new(py);
            // TODO: this needs to be fixed
            for change in &node.change_values_used_in_execution {
                if let Some(v) = &change.change_value {
                    match v {
                        ChangeValue { path: Some(path), value: Some(value), .. } => {
                            add_to_dict(
                                py,
                                &path.address,
                                SerializedValueWrapper(value.clone()).to_object(py),
                                &event_dict,
                            ).unwrap();
                        },
                        _ => {}
                    }
                }
            }

            dict.set_item("event", event_dict).unwrap();
        }
        dict.into_py(py)
    }
}

pub struct PyRespondPollNodeWillExecuteEvents(Response<RespondPollNodeWillExecuteEvents>);

impl IntoPy<Py<PyAny>> for PyRespondPollNodeWillExecuteEvents {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let PyRespondPollNodeWillExecuteEvents(resp) = self;
        let res = resp.into_inner();
        let RespondPollNodeWillExecuteEvents { node_will_execute_events } = res;
        let x: Vec<NodeWillExecuteOnBranchWrapper> = node_will_execute_events.iter().cloned().map(NodeWillExecuteOnBranchWrapper).collect();
        PyList::new(py, x).into_py(py)
    }
}


async fn get_client(url: String) -> Result<ExecutionRuntimeClient<tonic::transport::Channel>, PyErrWrapper> {
    ExecutionRuntimeClient::connect(url.clone()).await.map_err(PyErrWrapper::from)
}

// TODO: return a handle to nodes so that we can understand and inspect them
// TODO: include a __repr__ method on those nodes

#[pyclass]
#[derive(Clone)]
struct PyNodeHandle {
    n: NodeHandle,
}

impl PyNodeHandle {
    fn from(node_handle: NodeHandle) -> anyhow::Result<PyNodeHandle> {
        Ok(PyNodeHandle { n: node_handle })
    }
}

#[pymethods]
impl PyNodeHandle {
    fn get_name(&self) -> String {
        self.n.get_name()
    }

    /// This updates the definition of this node to query for the target NodeHandle's output. Moving forward
    /// it will execute whenever the target node resolves.
    fn run_when<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, graph_builder: &mut PyGraphBuilder, other_node_handle: PyNodeHandle) -> PyResult<&'a PyAny> {
        let mut n = self_.n.clone();
        let g = Arc::clone(&graph_builder.g);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            Ok(n.run_when(&mut graph_builder, &other_node_handle.n)
                .map_err(AnyhowErrWrapper)?)
        })
    }

    // #[pyo3(signature = (branch=0, frame=0))]
    // fn query<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, file_id: String, url: String, branch: u64, frame: u64) -> PyResult<&'a PyAny> {
    //     pyo3_asyncio::tokio::future_into_py(py, async move {
    //         Ok(PyQueryAtFrameResponse(self_.n.query(file_id, url, branch, frame)
    //             .await.map_err(PyErrWrapper::from)?))
    //     })
    // }

    fn __str__(&self) -> PyResult<String>   {
        // TODO: best practice is that these could be used to re-construct the same object
        let name = self.get_name();
        Ok(format!("NodeHandle(node={})", name))
    }

    fn __repr__(&self) -> PyResult<String> {
        let name = self.get_name();
        Ok(format!("NodeHandle(node={})", name))
    }
}


// TODO: all operations only apply to a specific branch at a time
// TODO: maintain an internal map of the generated change responses for node additions to the associated query necessary to get that result
#[pyclass(name="Chidori")]
struct PyChidori {
    c: Arc<Mutex<Chidori>>,
    file_id: String,
    current_head: u64,
    current_branch: u64,
    url: String
}

// TODO: internally all operations should have an assigned counter
//       we can keep the actual target counter hidden from the host sdk
#[pymethods]
impl PyChidori {

    #[new]
    #[pyo3(signature = (file_id=String::from("0"), url=String::from("http://127.0.0.1:9800"), api_token=None))]
    fn new(file_id: String, url: String, api_token: Option<String>) -> Self {
        debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, api_token);
        let c = Chidori::new(file_id.clone(), url.clone());
        PyChidori {
            c: Arc::new(Mutex::new(c)),
            file_id,
            current_head: 0,
            current_branch: 0,
            url,
        }
    }

    fn start_server<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, file_path: Option<String>) -> PyResult<&'a PyAny> {
        let c = Arc::clone(&self_.c);
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let c = c.lock().await;
            c.start_server(file_path).await.map_err(AnyhowErrWrapper)?;
            Ok(())
        })
    }

    #[pyo3(signature = (branch=0, frame=0))]
    fn play<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, branch: u64, frame: u64) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyResponseExecutionStatus(client.play(RequestAtFrame {
                id: file_id,
                frame,
                branch,
            }).await.map_err(PyErrWrapper::from)?))
        })
    }


    #[pyo3(signature = (frame=0))]
    fn pause<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, frame: u64) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyResponseExecutionStatus(client.pause(RequestAtFrame {
                id: file_id,
                frame,
                branch,
            }).await.map_err(PyErrWrapper::from)?))
        })
    }


    fn branch<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let result_branch = client.branch(RequestNewBranch {
                id: file_id,
                source_branch_id: branch,
                diverges_at_counter: 0,
            }).await.map_err(PyErrWrapper::from)?;
            // TODO: need to somehow handle writing to the current_branch
            Ok(PyResponseExecutionStatus(result_branch))
        })
    }

    #[pyo3(signature = (query=String::new(), branch=0, frame=0))]
    fn query<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, query: String, branch: u64, frame: u64) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyQueryAtFrameResponse(client.run_query(QueryAtFrame {
                id: file_id,
                query: Some(Query {
                    query: Some(query)
                }),
                frame,
                branch,
            }).await.map_err(PyErrWrapper::from)?))
        })
    }

    fn list_branches<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyListBranchesRes(client.list_branches(RequestListBranches {
                id: file_id,
            }).await.map_err(PyErrWrapper::from)?))
        })
    }

    fn display_graph_structure<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let file = client.current_file_state(RequestOnlyId {
                id: file_id,
                branch
            }).await.map_err(PyErrWrapper::from)?;
            let mut file = file.into_inner();
            let mut g = CleanedDefinitionGraph::zero();
            g.merge_file(&mut file).unwrap();
            Ok(g.get_dot_graph())
        })
    }

    // TODO: some of these register handlers instead
    // TODO: list registered graphs should not stream
    // TODO: add a message that sends the current graph state

    fn list_registered_graphs<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let resp = client.list_registered_graphs(Empty {
            }).await.map_err(PyErrWrapper::from)?;
            let mut stream = resp.into_inner();
            while let Some(x) = stream.next().await {
                // callback.call(py, (x,), None);
                info!("Registered Graph = {:?}", x);
            };
            Ok(())
        })
    }

    fn list_input_proposals<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        callback: PyObject
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let resp = client.list_input_proposals(RequestOnlyId {
                id: file_id,
                branch,
            }).await.map_err(PyErrWrapper::from)?;
            let mut stream = resp.into_inner();
            while let Some(x) = stream.next().await {
                // callback.call(py, (x,), None);
                info!("InputProposals = {:?}", x);
            };
            Ok(())
        })
    }

    // fn respond_to_input_proposal(mut self_: PyRefMut<'_, Self>) -> PyResult<()> {
    //     Ok(())
    // }

    // TODO: this is a successful callback example
    fn list_change_events<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        callback: PyObject
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let resp = client.list_change_events(RequestOnlyId {
                id: file_id,
                branch,
            }).await.map_err(PyErrWrapper::from)?;
            let mut stream = resp.into_inner();
            while let Some(x) = stream.next().await {
                Python::with_gil(|py| pyo3_asyncio::tokio::into_future(callback.as_ref(py).call((x.map(ChangeValueWithCounterWrapper).map_err(PyErrWrapper::from)?,), None)?))?
                    .await?;
            };
            Ok(())
        })
    }

    pub fn register_custom_node_handle<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        key: String,
        handler: PyObject
    ) -> PyResult<&'a PyAny> {
        let c = Arc::clone(&self_.c);
        let handler = Arc::new(handler);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut c = c.lock().await;
            c.register_custom_node_handle(key, Handler::new(
                move |n| {
                    let handler_clone = Arc::clone(&handler);
                    Box::pin(async move {
                        let result = Python::with_gil(|py|  {
                            let fut = handler_clone.as_ref().call(py, (NodeWillExecuteOnBranchWrapper(n).to_object(py), ), None)?;
                            pyo3_asyncio::tokio::into_future(fut.as_ref(py))
                        })?.await;
                        match result {
                            Ok(py_obj) => {
                                Python::with_gil(|py|  {
                                    let json_value = py_to_json(py, py_obj.as_ref(py));
                                    Ok(json_value)
                                })
                            },
                            Err(err) => Err(anyhow::Error::new(err)),
                        }
                    })
                }
            ));
            Ok(())
        })
    }

    fn run_custom_node_loop<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
    ) -> PyResult<&'a PyAny> {
        let c = Arc::clone(&self_.c);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut c = c.lock().await;
            Ok(c.run_custom_node_loop().await.map_err(AnyhowErrWrapper)?)
        })
    }


    //
    // fn observation_node(mut self_: PyRefMut<'_, Self>, name: String, query_def: Option<String>, template: String, model: String) -> PyResult<()> {
    //     let file_id = self_.file_id.clone();
    //     let node = create_observation_node(
    //         "".to_string(),
    //         None,
    //         "".to_string(),
    //     );
    //     executor::block_on(self_.client.merge(RequestFileMerge {
    //         id: file_id,
    //         file: Some(File {
    //             nodes: vec![node],
    //             ..Default::default()
    //         }),
    //         branch: 0,
    //     }));
    //     Ok(())
    // }
}

#[pyclass(name="GraphBuilder")]
#[derive(Clone)]
struct PyGraphBuilder {
    g: Arc<Mutex<GraphBuilder>>,
}

#[pymethods]
impl PyGraphBuilder {

    #[new]
    fn new() -> Self {
        let g = GraphBuilder::new();
        PyGraphBuilder {
            g: Arc::new(Mutex::new(g)),
        }
    }

    // https://github.com/PyO3/pyo3/issues/525
    #[pyo3(signature = (name=String::new(), queries=vec!["None".to_string()], output_tables=vec![], output=String::from("type O {}"), node_type_name=String::new()))]
    fn custom_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Option<Vec<String>>,
        output_tables: Vec<String>,
        output: String,
        node_type_name: String,
    ) -> PyResult<&'a PyAny> {
        let g = Arc::clone(&self_.g);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            let nh = graph_builder.custom_node(CustomNodeCreateOpts {
                name,
                queries,
                output_tables: Some(output_tables),
                output: Some(output),
                node_type_name,
            }).map_err(AnyhowErrWrapper)?;
            Ok(PyNodeHandle::from(nh).map_err(AnyhowErrWrapper)?)
        })
    }

    #[pyo3(signature = (name=String::new(), queries=vec!["None".to_string()], output_tables=None, output=None, code=String::new(), is_template=None))]
    fn deno_code_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Option<Vec<String>>,
        output_tables: Option<Vec<String>>,
        output: Option<String>,
        code: String,
        is_template: Option<bool>
    ) -> PyResult<&'a PyAny> {
        let g = Arc::clone(&self_.g);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            let nh = graph_builder.deno_code_node(DenoCodeNodeCreateOpts {
                name,
                queries,
                output_tables,
                output,
                code,
                is_template,
            }).map_err(AnyhowErrWrapper)?;
            Ok(PyNodeHandle::from(nh).map_err(AnyhowErrWrapper)?)
        })
    }


    #[pyo3(signature = (name=String::new(), queries=vec!["None".to_string()], output_tables=vec![], output=String::from("type O { }"), template=String::new(), action="WRITE".to_string(), embedding_model="TEXT_EMBEDDING_ADA_002".to_string(), db_vendor="QDRANT".to_string(), collection_name=String::new()))]
    fn vector_memory_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Option<Vec<String>>,
        output_tables: Vec<String>,
        output: String,
        template: String,
        action: String, // READ / WRITE
        embedding_model: String,
        db_vendor: String,
        collection_name: String,
    ) -> PyResult<&'a PyAny> {
        let g = Arc::clone(&self_.g);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            let nh = graph_builder.vector_memory_node(VectorMemoryNodeCreateOpts {
                name,
                queries,
                output_tables: Some(output_tables),
                output: Some(output),
                template: Some(template),
                action: Some(action),
                embedding_model: Some(embedding_model),
                db_vendor: Some(db_vendor),
                collection_name,
            }).map_err(AnyhowErrWrapper)?;
            Ok(PyNodeHandle::from(nh).map_err(AnyhowErrWrapper)?)
        })
    }


    // // TODO: nodes that are added should return a clean definition of what their addition looks like
    // // TODO: adding a node should also display any errors
    // /// x = None
    // /// with open("/Users/coltonpierson/Downloads/files_and_dirs.zip", "rb") as zip_file:
    // ///     contents = zip_file.read()
    // ///     x = await p.load_zip_file("LoadZip", """ output: String """, contents)
    // /// x
    // #[pyo3(signature = (name=String::new(), output_tables=vec![], output=String::new(), bytes=vec![]))]
    // fn load_zip_file<'a>(
    //     mut self_: PyRefMut<'_, Self>,
    //     py: Python<'a>,
    //     name: String,
    //     output_tables: Vec<String>,
    //     output: String,
    //     bytes: Vec<u8>
    // ) -> PyResult<&'a PyAny> {
    //     let file_id = self_.file_id.clone();
    //     let url = self_.url.clone();
    //     pyo3_asyncio::tokio::future_into_py(py, async move {
    //         let node = create_loader_node(
    //             name,
    //             vec![],
    //             output,
    //             LoadFrom::ZipfileBytes(bytes),
    //             output_tables
    //         );
    //         Ok(push_file_merge(&url, &file_id, node).await?)
    //     })
    // }

    // TODO: nodes that are added should return a clean definition of what their addition looks like
    // TODO: adding a node should also display any errors
    #[pyo3(signature = (name=String::new(), queries=vec!["None".to_string()], output_tables=None, template=String::new(), model=None))]
    fn prompt_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Option<Vec<String>>,
        output_tables: Option<Vec<String>>,
        template: String,
        model: Option<String>
    ) -> PyResult<&'a PyAny> {
        let g = Arc::clone(&self_.g);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            let nh = graph_builder.prompt_node(PromptNodeCreateOpts {
                name,
                queries,
                output_tables,
                template,
                model,
            }).map_err(AnyhowErrWrapper)?;
            Ok(PyNodeHandle::from(nh).map_err(AnyhowErrWrapper)?)
        })
    }

    fn commit<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        c: PyRef<'_, PyChidori>,
        branch: u64
    ) -> PyResult<&'a PyAny> {
        let g = Arc::clone(&self_.g);
        let c = Arc::clone(&c.c);
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut graph_builder = g.lock().await;
            let mut chidori = c.lock().await;
            let exec_status = graph_builder.commit(&chidori, branch).await
                .map(PyExecutionStatus)
                .map_err(AnyhowErrWrapper)?;
            Ok(exec_status)
        })
    }
}


/// A Python module implemented in Rust. The name of this function must match
/// the `lib.name` setting in the `Cargo.toml`, else Python will not be able to
/// import the module.
#[pymodule]
#[pyo3(name = "chidori")]
fn chidori(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    // pyo3_log::init();
    m.add_class::<PyChidori>()?;
    m.add_class::<PyGraphBuilder>()?;
    Ok(())
}

