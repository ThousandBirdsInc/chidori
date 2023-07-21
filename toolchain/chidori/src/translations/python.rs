use std::collections::VecDeque;
use pyo3::exceptions;
use pyo3::prelude::*;
use tonic::{Response, Status};
use futures::executor;
use futures::StreamExt;
use pyo3::types::{PyDict, PyList, PyString};
use pyo3::types::{PyBool, PyFloat, PyInt};
use pyo3::prelude::*;
use std::collections::HashMap;
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

pub struct PyExecutionStatus(Response<ExecutionStatus>);


impl IntoPy<Py<PyAny>> for PyExecutionStatus {
    fn into_py(self, py: Python) -> Py<PyAny> {
        let PyExecutionStatus(resp) = self;
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


// TODO: add an api for wiring together a system ot nodes
// TODO: require the description of the complete system, in order to avoid mutably handling changing edges
// TODO: system description must be fully encapsulated in a single object and applied all at once

#[pyclass]
#[derive(Clone)]
struct NodeHandle {
    url: String,
    file_id: String,
    node: Item,
    exec_status: ExecutionStatus,
    indiv: CleanIndividualNode
}

impl NodeHandle {
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

#[pymethods]
impl NodeHandle {
    fn get_name(&self) -> String {
        self.node.core.as_ref().unwrap().name.clone()
    }

    /// This updates the definition of this node to query for the target NodeHandle's output. Moving forward
    /// it will execute whenever the target node resolves.
    #[pyo3(signature = (node_handle=None))]
    fn run_when<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, node_handle: Option<NodeHandle>) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let mut node = self_.node.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            // TODO: scan the queries and don't insert if the query already exists
            if let Some(node_handle) = node_handle {
                let queries = &mut node.core.as_mut().unwrap().queries;
                let q = construct_query_from_output_type(
                    &node_handle.get_name(),
                    &node_handle.get_name(),
                    &node_handle.indiv.output_path
                ).map_err(AnyhowErrWrapper)?;
                queries.push(Query { query: Some(q)});
                Ok(push_file_merge(&url, &file_id, node).await?)
            } else {
                Err(PyErr::new::<PyTypeError, _>("node_handle must be a NodeHandle"))
            }
        })
    }

    #[pyo3(signature = (branch=0, frame=0))]
    fn query<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, branch: u64, frame: u64) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let name = self_.get_name();

        let query = construct_query_from_output_type(&name, &name, &self_.indiv.output_path)
            .map_err(AnyhowErrWrapper)?;

        // TODO: we need this to watch for changes to the query instead
        // TODO: or this should await until either the target counter has elapsed or the query has resulted

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

    fn __str__(&self) -> PyResult<String>   {
        // TODO: best practice is that these could be used to re-construct the same object
        let name = self.get_name();
        Ok(format!("NodeHandle(file_id={}, node={})", self.file_id, name))
    }

    fn __repr__(&self) -> PyResult<String> {
        let name = self.get_name();
        Ok(format!("NodeHandle(file_id={}, node={})", self.file_id, name))
    }
}


// TODO: all operations only apply to a specific branch at a time
// TODO: maintain an internal map of the generated change responses for node additions to the associated query necessary to get that result
#[pyclass]
struct Chidori {
    file_id: String,
    current_head: u64,
    current_branch: u64,
    url: String
}



async fn push_file_merge(url: &String, file_id: &String, node: Item) -> Result<NodeHandle, PyErr> {
    let mut client = get_client(url.clone()).await?;
    let exec_status = client.merge(RequestFileMerge {
        id: file_id.clone(),
        file: Some(File {
            nodes: vec![node.clone()],
            ..Default::default()
        }),
        branch: 0,
    }).await.map_err(PyErrWrapper::from)?.into_inner();
    Ok(NodeHandle::from(
        url.clone(),
        file_id.clone(),
        node,
        exec_status
    ).map_err(AnyhowErrWrapper)?)
}

// TODO: internally all operations should have an assigned counter
//       we can keep the actual target counter hidden from the host sdk
#[pymethods]
impl Chidori {

    #[new]
    #[pyo3(signature = (file_id=String::from("0"), url=String::from("http://127.0.0.1:9800"), api_token=None))]
    fn new(file_id: String, url: String, api_token: Option<String>) -> Self {
        debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, api_token);
        Chidori {
            file_id,
            current_head: 0,
            current_branch: 0,
            url,
        }
    }

    fn start_server<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, file_path: Option<String>) -> PyResult<&'a PyAny> {
        let url_server = self_.url.clone();
        std::thread::spawn(move || {
            let result = run_server(url_server, file_path);
            match result {
                Ok(_) => { },
                Err(e) => {
                    println!("Error running server: {}", e);
                },
            }
        });

        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            'retry: loop {
                let client = get_client(url.clone());
                match client.await {
                    Ok(connection) => {
                        eprintln!("Connection successfully established {:?}", &url);
                        break 'retry
                    },
                    Err(e) => {
                        eprintln!("Error connecting to server: {} with Error {}. Retrying...", &url, &e.0);
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                    }
                }
            }
            Ok(())
        })
    }

    #[pyo3(signature = (branch=0, frame=0))]
    fn play<'a>(mut self_: PyRefMut<'_, Self>, py: Python<'a>, branch: u64, frame: u64) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyExecutionStatus(client.play(RequestAtFrame {
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
            Ok(PyExecutionStatus(client.pause(RequestAtFrame {
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
            Ok(PyExecutionStatus(result_branch))
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


    // TODO: nodes that are added should return a clean definition of what their addition looks like
    // TODO: adding a node should also display any errors
    /// x = None
    /// with open("/Users/coltonpierson/Downloads/files_and_dirs.zip", "rb") as zip_file:
    ///     contents = zip_file.read()
    ///     x = await p.load_zip_file("LoadZip", """ output: String """, contents)
    /// x
    #[pyo3(signature = (name=String::new(), output_tables=vec![], output=String::new(), bytes=vec![]))]
    fn load_zip_file<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        output_tables: Vec<String>,
        output: String,
        bytes: Vec<u8>
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let node = create_loader_node(
                name,
                vec![],
                output,
                LoadFrom::ZipfileBytes(bytes),
                output_tables
            );
            Ok(push_file_merge(&url, &file_id, node).await?)
        })
    }

    // TODO: nodes that are added should return a clean definition of what their addition looks like
    // TODO: adding a node should also display any errors
    #[pyo3(signature = (name=String::new(), queries=vec![None], output_tables=vec![], template=String::new(), model=String::from("GPT_3_5_TURBO")))]
    fn prompt_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Vec<Option<String>>,
        output_tables: Vec<String>,
        template: String,
        model: String
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let node = create_prompt_node(
                name,
                queries,
                template,
                model,
                output_tables
            ).map_err(PyErrWrapper::from)?;
            Ok(push_file_merge(&url, &file_id, node).await?)
        })
    }

    fn poll_local_code_node_execution<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            let result = client.poll_node_will_execute_events(FilteredPollNodeWillExecuteEventsRequest {
                id: file_id.clone(),
            }).await.map_err(PyErrWrapper::from)?;
            debug!("poll_local_code_node_execution result = {:?}", result);
            Ok(PyRespondPollNodeWillExecuteEvents(result))
        })
    }

    #[pyo3(signature = (branch=0, counter=0))]
    fn ack_local_code_node_execution<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        branch: u64,
        counter: u64,
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;
            Ok(PyExecutionStatus(client.ack_node_will_execute_event(RequestAckNodeWillExecuteEvent {
                id: file_id.clone(),
                branch,
                counter,
            }).await.map_err(PyErrWrapper::from)?))
        })
    }

    #[pyo3(signature = (branch=0, counter=0, node_name=String::new(), response=None))]
    fn respond_local_code_node_execution<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        branch: u64,
        counter: u64,
        node_name: String,
        response: Option<PyObject>
    ) -> PyResult<&'a PyAny> {

        let file_id = self_.file_id.clone();
        let url = self_.url.clone();

        // TODO: need parent counters from the original change
        // TODO: need source node

        let response_paths = if let Some(response) = response {
            dict_to_paths(py, response.downcast::<PyDict>(py)?)?
        } else {
            vec![]
        };

        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut client = get_client(url).await?;

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
            Ok(PyExecutionStatus(client.push_worker_event(FileAddressedChangeValueWithCounter {
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
            }).await.map_err(PyErrWrapper::from)?))
        })
    }

    // TODO: handle dispatch to this handler - should accept a callback
    // https://github.com/PyO3/pyo3/issues/525
    #[pyo3(signature = (name=String::new(), queries=vec![None], output_tables=vec![], output=String::from("type O {}"), node_type_name=String::new()))]
    fn custom_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Vec<Option<String>>,
        output_tables: Vec<String>,
        output: String,
        node_type_name: String,
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch;
        pyo3_asyncio::tokio::future_into_py(py, async move {
            // Register the node with the system
            let node = create_custom_node(
                name,
                queries,
                output,
                node_type_name,
                output_tables
            );
            Ok(push_file_merge(&url, &file_id, node).await?)
        })
    }

    #[pyo3(signature = (name=String::new(), queries=vec![None], output_tables=vec![], output=String::from("type O { output: String }"), code=String::new(), is_template=false))]
    fn deno_code_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Vec<Option<String>>,
        output_tables: Vec<String>,
        output: String,
        code: String,
        is_template: bool
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let node = create_code_node(
                name,
                queries,
                output,
                SourceNodeType::Code("DENO".to_string(), code, is_template),
                output_tables
            );
            Ok(push_file_merge(&url, &file_id, node).await?)
        })
    }


    #[pyo3(signature = (name=String::new(), queries=vec![None], output_tables=vec![], output=String::from("type O { }"), template=String::new(), action="WRITE".to_string(), embedding_model="TEXT_EMBEDDING_ADA_002".to_string(), db_vendor="QDRANT".to_string(), collection_name=String::new()))]
    fn vector_memory_node<'a>(
        mut self_: PyRefMut<'_, Self>,
        py: Python<'a>,
        name: String,
        queries: Vec<Option<String>>,
        output_tables: Vec<String>,
        output: String,
        template: String,
        action: String, // READ / WRITE
        embedding_model: String,
        db_vendor: String,
        collection_name: String,
    ) -> PyResult<&'a PyAny> {
        let file_id = self_.file_id.clone();
        let url = self_.url.clone();
        let branch = self_.current_branch.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            let node = create_vector_memory_node(
                name,
                queries,
                output,
                action,
                embedding_model,
                template,
                db_vendor,
                collection_name,
                output_tables
            ).map_err(PyErrWrapper::from)?;
            Ok(push_file_merge(&url, &file_id, node).await?)
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


/// A Python module implemented in Rust. The name of this function must match
/// the `lib.name` setting in the `Cargo.toml`, else Python will not be able to
/// import the module.
#[pymodule]
#[pyo3(name = "chidori")]
fn chidori(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    pyo3_log::init();
    m.add_class::<Chidori>()?;
    Ok(())
}