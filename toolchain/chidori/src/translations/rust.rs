use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use anyhow::Error;
use futures::future::BoxFuture;
use futures::StreamExt;
use log::{debug, info};
use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;
use prompt_graph_core::build_runtime_graph::graph_parse::{CleanedDefinitionGraph, CleanIndividualNode, construct_query_from_output_type, derive_for_individual_node};
use prompt_graph_core::graph_definition::{create_code_node, create_custom_node, create_prompt_node, create_vector_memory_node, SourceNodeType};
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, Empty, ExecutionStatus, File, FileAddressedChangeValueWithCounter, FilteredPollNodeWillExecuteEventsRequest, Item, ListBranchesRes, NodeWillExecute, NodeWillExecuteOnBranch, Path, Query, QueryAtFrame, QueryAtFrameResponse, RequestAckNodeWillExecuteEvent, RequestAtFrame, RequestFileMerge, RequestListBranches, RequestNewBranch, RequestOnlyId, RespondPollNodeWillExecuteEvents, SerializedValue, SerializedValueArray, SerializedValueObject};
use prompt_graph_core::proto2::execution_runtime_client::ExecutionRuntimeClient;
use prompt_graph_core::proto2::serialized_value::Val;
use prompt_graph_exec::tonic_runtime::run_server;
use neon_serde3;
use serde::{Deserialize, Serialize};
use tonic::Status;
use prompt_graph_core::templates::json_value_to_serialized_value;
use crate::translations::shared::json_value_to_paths;
pub use prompt_graph_core::utils::serialized_value_to_string;

async fn get_client(url: String) -> Result<ExecutionRuntimeClient<tonic::transport::Channel>, tonic::transport::Error> {
    ExecutionRuntimeClient::connect(url.clone()).await
}

type CallbackHandler = Box<dyn Fn(NodeWillExecuteOnBranch) -> BoxFuture<'static, anyhow::Result<serde_json::Value>> + Send + Sync>;

pub struct Handler {
    pub(crate) callback: CallbackHandler
}

impl Handler {
    pub fn new<F>(f: F) -> Self
        where
            F: Fn(NodeWillExecuteOnBranch) -> BoxFuture<'static, anyhow::Result<serde_json::Value>> + Send + Sync + 'static
    {
        Handler {
            callback: Box::new(f),
        }
    }
}


#[derive(Clone)]
pub struct Chidori {
    file_id: String,
    current_head: u64,
    current_branch: u64,
    url: String,
    pub(crate) custom_node_handlers: HashMap<String, Arc<Handler>>
}

impl Chidori {

    pub fn new(file_id: String, url: String) -> Self {
        if !url.contains("://") {
            panic!("Invalid url, must include protocol");
        }
        // let api_token = cx.argument_opt(2)?.value(&mut cx);
        debug!("Creating new Chidori instance with file_id={}, url={}, api_token={:?}", file_id, url, "".to_string());
        Chidori {
            file_id,
            current_head: 0,
            current_branch: 0,
            url,
            custom_node_handlers: HashMap::new(),
        }
    }

    pub async fn start_server(&self, file_path: Option<String>) -> anyhow::Result<()> {
        let url_server = self.url.clone();
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

        let url = self.url.clone();
        loop {
            match get_client(url.clone()).await {
                Ok(connection) => {
                    eprintln!("Connection successfully established {:?}", &url);
                    return Ok(());
                },
                Err(e) => {
                    eprintln!("Error connecting to server: {} with Error {}. Retrying...", &url, &e.to_string());
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
        }
    }

    pub async fn play(&self, branch: u64, frame: u64) -> anyhow::Result<ExecutionStatus> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let result = client.play(RequestAtFrame {
            id: file_id,
            frame,
            branch,
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn pause(&self, frame: u64) -> anyhow::Result<ExecutionStatus> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let branch = self.current_branch.clone();

        let mut client = get_client(url).await?;
        let result = client.pause(RequestAtFrame {
            id: file_id,
            frame,
            branch,
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn query( &self, query: String, branch: u64, frame: u64, ) -> anyhow::Result<QueryAtFrameResponse> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let result = client.run_query(QueryAtFrame {
            id: file_id,
            query: Some(Query {
                query: Some(query)
            }),
            frame,
            branch,
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn branch( &self, branch: u64, frame: u64, ) -> anyhow::Result<ExecutionStatus> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let result = client.branch(RequestNewBranch {
            id: file_id,
            source_branch_id: branch,
            diverges_at_counter: frame
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn list_branches( &self) -> anyhow::Result<ListBranchesRes> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let result = client.list_branches(RequestListBranches {
            id: file_id,
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn display_graph_structure( &self, branch: u64) -> anyhow::Result<String> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let file = client.current_file_state(RequestOnlyId {
            id: file_id,
            branch
        }).await?;
        let mut file = file.into_inner();
        let mut g = CleanedDefinitionGraph::zero();
        g.merge_file(&mut file).unwrap();
        Ok(g.get_dot_graph())
    }

    pub async fn list_registered_graphs(&self) -> anyhow::Result<()> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let resp = client.list_registered_graphs(Empty { }).await?;
        let mut stream = resp.into_inner();
        while let Some(x) = stream.next().await {
            // callback.call(py, (x,), None);
            info!("Registered Graph = {:?}", x);
        };
        Ok(())
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
//
//     // TODO: this should accept an "Object" instead of args
//     // TODO: nodes that are added should return a clean definition of what their addition looks like
//     // TODO: adding a node should also display any errors

    pub fn register_custom_node_handle(&mut self, key: String, handler: Handler) {
        self.custom_node_handlers.insert(key, Arc::new(handler));
    }

    pub async fn poll_local_code_node_execution(&self) -> anyhow::Result<RespondPollNodeWillExecuteEvents> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let req = FilteredPollNodeWillExecuteEventsRequest { id: file_id.clone() };
        let result = client.poll_custom_node_will_execute_events(req).await?;
        Ok(result.into_inner())
    }

    pub async fn ack_local_code_node_execution(&self, branch: u64, counter : u64) -> anyhow::Result<ExecutionStatus> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();
        let mut client = get_client(url).await?;
        let result = client.ack_node_will_execute_event(RequestAckNodeWillExecuteEvent {
            id: file_id.clone(),
            branch,
            counter,
        }).await?;
        Ok(result.into_inner())
    }

    pub async fn respond_local_code_node_execution<T: Serialize>(
        &self,
        branch: u64,
        counter: u64,
        node_name: String,
        response: T
    ) -> anyhow::Result<ExecutionStatus> {
        let file_id = self.file_id.clone();
        let url = self.url.clone();

        let json_object = serde_json::to_value(response)?;
        let response_paths = json_value_to_paths(&json_object);
        let filled_values = response_paths.into_iter().map(|path| {
            ChangeValue {
                path: Some(Path {
                    address: path.0,
                }),
                value: Some(path.1),
                branch,
            }
        }).collect();

        // TODO: need parent counters from the original change
        // TODO: need source node
        let mut client = get_client(url).await?;

        // TODO: need to add the output table paths to these
        // TODO: this needs to look more like a real change
        Ok(client.push_worker_event(FileAddressedChangeValueWithCounter {
            branch,
            counter,
            node_name: node_name.clone(),
            id: file_id.clone(),
            change: Some(ChangeValueWithCounter {
                filled_values,
                parent_monotonic_counters: vec![],
                monotonic_counter: counter,
                branch,
                source_node: node_name.clone(),
            })
        }).await?.into_inner())
    }

    pub async fn run_custom_node_loop(&self) -> anyhow::Result<()> {
        loop {
            let mut backoff = 2;
            let events = self.poll_local_code_node_execution().await?;
            if events.node_will_execute_events.len() <= 0 {
                backoff = backoff * backoff;
                tokio::time::sleep(std::time::Duration::from_millis(100 * backoff)).await;
                continue;
            } else {
                backoff = 2;
                for ev  in &events.node_will_execute_events {
                    // ACK messages
                    let NodeWillExecuteOnBranch { branch, counter, node, ..} = ev;
                    let node_name = &node.as_ref().unwrap().source_node;
                    if let Some(x) = self.custom_node_handlers.get(&ev.custom_node_type_name.clone().unwrap()) {
                        self.ack_local_code_node_execution(*branch, *counter).await?;
                        let result = (x.as_ref().callback)(ev.clone()).await?;
                        dbg!(&result);
                        self.respond_local_code_node_execution(*branch, *counter, node_name.clone(), result).await?;
                    }
                }
            }
        }
    }
}


fn default_queries() -> Option<Vec<String>> {
    Some(vec!["None".to_string()])
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PromptNodeCreateOpts {
    pub name: String,
    pub queries: Option<Vec<String>>,
    pub output_tables: Option<Vec<String>>,
    pub template: String,
    pub model: Option<String>
}

impl Default for PromptNodeCreateOpts {
    fn default() -> Self {
        PromptNodeCreateOpts {
            name: "".to_string(),
            queries: default_queries(),
            output_tables: None,
            template: "".to_string(),
            model: Some("GPT_3_5_TURBO".to_string()),
        }
    }
}
impl PromptNodeCreateOpts {
    pub fn merge(&mut self, other: PromptNodeCreateOpts) {
        self.name = other.name;
        self.queries = other.queries.or(self.queries.take());
        self.output_tables = other.output_tables.or(self.output_tables.take());
        self.template = other.template;
        self.model = other.model.or(self.model.take());
    }
}



#[derive(serde::Serialize, serde::Deserialize)]
pub struct CustomNodeCreateOpts {
    pub name: String,
    pub queries: Option<Vec<String>>,
    pub output_tables: Option<Vec<String>>,
    pub output: Option<String>,
    pub node_type_name: String
}


impl Default for CustomNodeCreateOpts {
    fn default() -> Self {
        CustomNodeCreateOpts {
            name: "".to_string(),
            queries: default_queries(),
            output_tables: None,
            output: None,
            node_type_name: "".to_string(),
        }
    }
}
impl CustomNodeCreateOpts {
    pub fn merge(&mut self, other: CustomNodeCreateOpts) {
        self.name = other.name;
        self.queries = other.queries.or(self.queries.take());
        self.output_tables = other.output_tables.or(self.output_tables.take());
        self.output = other.output.or(self.output.take());
        self.node_type_name = other.node_type_name;
    }
}



#[derive(serde::Serialize, serde::Deserialize)]
pub struct DenoCodeNodeCreateOpts {
    pub name: String,
    pub queries: Option<Vec<String>>,
    pub output_tables: Option<Vec<String>>,
    pub output: Option<String>,
    pub code: String,
    pub is_template: Option<bool>
}

impl Default for DenoCodeNodeCreateOpts {
    fn default() -> Self {
        DenoCodeNodeCreateOpts {
            name: "".to_string(),
            queries: default_queries(),
            output_tables: None,
            output: None,
            code: "".to_string(),
            is_template: None,
        }
    }
}

impl DenoCodeNodeCreateOpts {
    pub fn merge(&mut self, other: DenoCodeNodeCreateOpts) {
        self.name = other.name;
        self.queries = other.queries.or(self.queries.take());
        self.output_tables = other.output_tables.or(self.output_tables.take());
        self.output = other.output.or(self.output.take());
        self.code = other.code;
        self.is_template = other.is_template.or(self.is_template.take());
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct VectorMemoryNodeCreateOpts {
    pub name: String,
    pub queries: Option<Vec<String>>,
    pub output_tables: Option<Vec<String>>,
    pub output: Option<String>,
    pub template: Option<String>, // TODO: default is the contents of the query
    pub action: Option<String>, // TODO: default WRITE
    pub embedding_model: Option<String>, // TODO: default TEXT_EMBEDDING_ADA_002
    pub db_vendor: Option<String>, // TODO: default QDRANT
    pub collection_name: String,
}


impl Default for VectorMemoryNodeCreateOpts {
    fn default() -> Self {
        VectorMemoryNodeCreateOpts {
            name: "".to_string(),
            queries: None,
            output_tables: None,
            output: None,
            template: None,
            action: None,
            embedding_model: None,
            db_vendor: None,
            collection_name: "".to_string(),
        }
    }
}


impl VectorMemoryNodeCreateOpts {
    pub fn merge(&mut self, other: VectorMemoryNodeCreateOpts) {
        self.name = other.name;
        self.queries = other.queries.or(self.queries.take());
        self.output_tables = other.output_tables.or(self.output_tables.take());
        self.output = other.output.or(self.output.take());
        self.template = other.template.or(self.template.take());
        self.action = other.action.or(self.action.take());
        self.embedding_model = other.embedding_model.or(self.embedding_model.take());
        self.db_vendor = other.db_vendor.or(self.db_vendor.take());
        self.collection_name = other.collection_name;
    }
}

fn remap_queries(queries: Option<Vec<String>>) -> Vec<Option<String>> {
    let queries: Vec<Option<String>> = if let Some(queries) = queries {
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
    queries
}

#[derive(Clone)]
pub struct GraphBuilder {
    clean_graph: CleanedDefinitionGraph,
}

impl GraphBuilder {
    pub fn new() -> Self {
        GraphBuilder {
            clean_graph: CleanedDefinitionGraph::zero()
        }
    }
    pub fn prompt_node(&mut self, arg: PromptNodeCreateOpts) -> anyhow::Result<NodeHandle> {
        let mut def = PromptNodeCreateOpts::default();
        def.merge(arg);
        let node = create_prompt_node(
            def.name.clone(),
            remap_queries(def.queries),
            def.template,
            def.model.unwrap_or("GPT_3_5_TURBO".to_string()),
            def.output_tables.unwrap_or(vec![]))?;
        self.clean_graph.merge_file(&File { nodes: vec![node.clone()], ..Default::default() })?;
        Ok(NodeHandle::from(node)?)
    }

    pub fn custom_node(&mut self, arg: CustomNodeCreateOpts) -> anyhow::Result<NodeHandle> {
        let mut def = CustomNodeCreateOpts::default();
        def.merge(arg);
        let node = create_custom_node(
            def.name.clone(),
            remap_queries(def.queries.clone()),
            def.output.unwrap_or("type O {}".to_string()),
            def.node_type_name,
            def.output_tables.unwrap_or(vec![])
        );
        self.clean_graph.merge_file(&File { nodes: vec![node.clone()], ..Default::default() })?;
        Ok(NodeHandle::from(node)?)
    }


    pub fn deno_code_node(&mut self, arg: DenoCodeNodeCreateOpts) -> anyhow::Result<NodeHandle> {
        let mut def = DenoCodeNodeCreateOpts::default();
        def.merge(arg);
        let node = create_code_node(
            def.name.clone(),
            remap_queries(def.queries.clone()),
            def.output.unwrap_or("type O {}".to_string()),
            SourceNodeType::Code("DENO".to_string(), def.code, def.is_template.unwrap_or(false)),
            def.output_tables.unwrap_or(vec![])
        );
        self.clean_graph.merge_file(&File { nodes: vec![node.clone()], ..Default::default() })?;
        Ok(NodeHandle::from(node)?)
    }


    pub fn vector_memory_node(&mut self, arg: VectorMemoryNodeCreateOpts) -> anyhow::Result<NodeHandle> {
        let mut def = VectorMemoryNodeCreateOpts::default();
        def.merge(arg);
        let node = create_vector_memory_node(
            def.name.clone(),
            remap_queries(def.queries.clone()),
            def.output.unwrap_or("type O {}".to_string()),
            def.action.unwrap_or("READ".to_string()),
            def.embedding_model.unwrap_or("TEXT_EMBEDDING_ADA_002".to_string()),
            def.template.unwrap_or("".to_string()),
            def.db_vendor.unwrap_or("QDRANT".to_string()),
            def.collection_name,
            def.output_tables.unwrap_or(vec![])
        )?;
        self.clean_graph.merge_file(&File { nodes: vec![node.clone()], ..Default::default() })?;
        Ok(NodeHandle::from(node)?)
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

    pub async fn commit(&self, c: &Chidori, branch: u64) -> anyhow::Result<ExecutionStatus> {
        let url = &c.url;
        let file_id = &c.file_id;
        let mut client = get_client(url.clone()).await?;
        let nodes = self.clean_graph.node_by_name.clone().into_values().collect();

        Ok(client.merge(RequestFileMerge {
            id: file_id.clone(),
            file: Some(File { nodes, ..Default::default() }),
            branch: 0,
        }).await.map(|x| x.into_inner())?)
    }
}


// Node handle
#[derive(Clone)]
pub struct NodeHandle {
    pub node: Item,
    indiv: CleanIndividualNode
}

impl NodeHandle {
    fn from(node: Item) -> anyhow::Result<NodeHandle> {
        let indiv = derive_for_individual_node(&node)?;
        Ok(NodeHandle {
            node,
            indiv
        })
    }
}


impl NodeHandle {
    pub(crate) fn get_name(&self) -> String {
        self.node.core.as_ref().unwrap().name.clone()
    }

    fn get_output_type(&self) -> Vec<Vec<String>> {
        self.indiv.output_paths.clone()
    }

    pub fn run_when(&mut self, graph_builder: &mut GraphBuilder, other_node: &NodeHandle) -> anyhow::Result<bool> {
        let queries = &mut self.node.core.as_mut().unwrap().queries;

        // Remove null query if it is the only one present
        if queries.len() == 1 && queries[0].query.is_none() {
            queries.remove(0);
        }

        let q = construct_query_from_output_type(
            &other_node.get_name(),
            &other_node.get_name(),
            &other_node.get_output_type()
        ).unwrap();
        queries.push(Query { query: Some(q)});
        graph_builder.clean_graph.merge_file(&File { nodes: vec![self.node.clone()], ..Default::default() })?;
        Ok(true)
    }


    pub async fn query(&self, file_id: String, url: String, branch: u64, frame: u64) -> anyhow::Result<HashMap<String, SerializedValue>> {
        let name = &self.node.core.as_ref().unwrap().name;
        let query = construct_query_from_output_type(&name, &name, &self.indiv.output_paths).unwrap();
        let mut client = get_client(url).await?;
        let result = client.run_query(QueryAtFrame {
            id: file_id,
            query: Some(Query {
                query: Some(query)
            }),
            frame,
            branch,
        }).await?;
        let res = result.into_inner();
        let mut obj = HashMap::new();
        for value in res.values.iter() {
            let c = value.change_value.as_ref().unwrap();
            let k = c.path.as_ref().unwrap().address.join(":");
            let v = c.value.as_ref().unwrap().clone();
            obj.insert(k, v).unwrap();
        }
        Ok(obj)
    }

}

#[macro_export]
macro_rules! register_node_handle {
    ($c:expr, $name:expr, $handler:expr) => {
        $c.register_custom_node_handle($name.to_string(), Handler::new(
            move |n| Box::pin(async move { ($handler)(n).await })
        ));
    };
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_graph() {
    }
}
