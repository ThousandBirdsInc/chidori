use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::thread::JoinHandle;

use dashmap::DashMap;
use futures_core::Stream;
use prost::Message;
use tokio::sync::{mpsc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming, transport::Server};
use tokio::task;

use prompt_graph_core::graph_definition::DefinitionGraph;
use prompt_graph_core::proto2::{ChangeValueWithCounter, Empty, ExecutionStatus, NodeWillExecuteOnBranch, File, FileAddressedChangeValueWithCounter, FilteredPollNodeWillExecuteEventsRequest, InputProposal, ListBranchesRes, NodeWillExecute, ParquetFile, QueryAtFrame, QueryAtFrameResponse, RequestAtFrame, RequestFileMerge, RequestInputProposalResponse, RequestListBranches, RequestNewBranch, RequestOnlyId, RespondPollNodeWillExecuteEvents, RequestAckNodeWillExecuteEvent, UpsertPromptLibraryRecord};
use prompt_graph_core::proto2::execution_runtime_server::{ExecutionRuntime, ExecutionRuntimeServer};

use log::{debug, error, info, warn};
use neon::macro_internal::runtime::buffer::new;
use sled::Event;
use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing_log::LogTracer;
use tracing_subscriber::{EnvFilter, fmt, FmtSubscriber, Layer};
use prompt_graph_core::execution_router::evaluate_changes_against_node;
use prompt_graph_core::build_runtime_graph::graph_parse::{query_path_from_query_string};

use tracing_chrome::ChromeLayerBuilder;
use tracing_flame::FlameLayer;
use tracing_subscriber::{registry::Registry, prelude::*};

use crate::db_operations::{get_change_counter_for_branch, insert_executor_file_existence_by_id};
use crate::db_operations::branches::{create_branch, create_root_branch, list_branches};
use crate::db_operations::changes::{insert_new_change_value_with_counter, scan_all_resolved_changes};
use crate::db_operations::input_proposals_and_responses::insert_input_response;
use crate::db_operations::playback::pause_execution_at_frame;
use crate::db_operations::playback::play_execution_at_frame;
use crate::db_operations::changes::scan_all_pending_changes;
use crate::db_operations::custom_node_execution::insert_custom_node_execution;
use crate::db_operations::graph_mutations::{insert_pending_graph_mutation, scan_all_file_mutations_on_branch};
use crate::db_operations::input_proposals_and_responses::scan_all_input_proposals;
use crate::db_operations::executing_nodes::{move_will_execute_event_to_complete, move_will_execute_event_to_in_progress, scan_all_custom_node_will_execute_events, scan_all_will_execute_pending_events, subscribe_to_will_execute_events_by_name};
use crate::db_operations::prompt_library::insert_prompt_library_mutation;

use crate::executor::{Executor, InternalStateHandler};



#[derive(Debug)]
pub struct MyExecutionRuntime {
    db: Arc<sled::Db>,
    executor_started: Arc<DashMap<String, bool>>
}

impl MyExecutionRuntime {
    fn new(file_path: Option<String>) -> Self {
        let db_config = sled::Config::default();
        let db_config = if let Some(path) = file_path {
            if path.contains(":memory:") {
                db_config.temporary(true)
            } else {
                db_config.path(path)
            }
        } else {
            db_config.path("/tmp/prompt-graph".to_string())
        };

        MyExecutionRuntime {
            db: Arc::new(db_config.open().unwrap()),
            executor_started: Arc::new(DashMap::new())
        }
    }

    fn get_tree(&self, id: &str) -> sled::Tree {
        let db = self.db.clone();
        db.open_tree(id).unwrap()
    }
}


#[tonic::async_trait]
impl ExecutionRuntime for MyExecutionRuntime {

    #[tracing::instrument]
    async fn run_query(&self, request: Request<QueryAtFrame>) -> Result<Response<QueryAtFrameResponse>, Status> {
        debug!("Received run_query request: {:?}", &request);
        let query = request.get_ref().query.as_ref().unwrap();
        let branch = request.get_ref().branch;
        let counter = request.get_ref().frame;
        let tree = self.get_tree(&request.get_ref().id.clone());
        let state = InternalStateHandler {
            tree: &tree,
            branch,
            counter
        };
        let paths = query_path_from_query_string(&query.query.clone().unwrap()).unwrap();
        if let Some(values) = evaluate_changes_against_node(&state, &paths) {
            Ok(Response::new(QueryAtFrameResponse {
                values
            }))
        } else {
            Ok(Response::new(QueryAtFrameResponse {
                values: vec![]
            }))
        }
    }

    /// Register a new execution graph with this execution runtime
    /// This kicks off a new executor in an async green-thread to avoid blocking Tonic.
    /// If there is already a file with this id in place, we perform a merge with that definition
    /// which is our mechanism for runtime mutations.
    #[tracing::instrument]
    async fn merge(&self, request: Request<RequestFileMerge>) -> Result<Response<ExecutionStatus>, Status> {
        // TODO: this needs to push the counter forward
        debug!("Received merge request: {:?}", request);
        let file = request.get_ref().file.as_ref().unwrap();
        let branch = request.get_ref().branch;
        let id = file.id.clone();
        let tree = self.get_tree(&request.get_ref().id.clone());
        insert_pending_graph_mutation(&tree, branch, file.clone());
        let monotonic_counter = get_change_counter_for_branch(&tree, branch);
        Ok(Response::new(ExecutionStatus{ id, monotonic_counter, branch}))
    }

    #[tracing::instrument]
    async fn current_file_state(&self, request: Request<RequestOnlyId>) -> Result<Response<File>, Status> {
        debug!("Received current_file_state request: {:?}", request);
        let tree = &self.get_tree(&request.get_ref().id.clone());
        let id = request.get_ref().id.clone();
        let branch = &request.get_ref().branch;
        let mutations = scan_all_file_mutations_on_branch(tree, *branch);
        let mut name_map = HashMap::new();
        let mut name_map_version_markers: HashMap<String, (u64, u64)> = HashMap::new();
        let mut new_file = File {
            id,
            nodes: vec![],
        };
        // TODO: filter to changes below the provided counter
        for (is_resolved, k, mutation) in mutations {
            for node in mutation.nodes {
                let node_insert = node.clone();
                let name = node.core.unwrap().name;
                if let Some(marker) = name_map_version_markers.get(&name) {
                    // overwrite and insert updated node if the counter is higher
                    if (*marker).1 < k.1 {
                        name_map_version_markers.insert(name.clone(), k);
                        name_map.insert(name.clone(), node_insert);
                    }
                } else {
                    name_map_version_markers.insert(name.clone(), k);
                    name_map.insert(name.clone(), node_insert);
                }
            }
        }
        // Push all resolved nodes into file
        for (_, node) in name_map {
            new_file.nodes.push(node);
        }
        Ok(Response::new(new_file))
    }

    #[tracing::instrument]
    async fn get_parquet_history(&self, request: Request<RequestOnlyId>) -> Result<Response<ParquetFile>, Status> {
        debug!("Received get_parquet_history request: {:?}", request);
        let tree = &self.get_tree(&request.get_ref().id.clone());
        // TODO: serialize the target branch to parquet
        todo!()
    }

    #[tracing::instrument]
    async fn play(&self, request: Request<RequestAtFrame>) -> Result<Response<ExecutionStatus>, Status> {
        // Play also behaves as our "Connect" message
        debug!("Received play request: {:?}", request);
        let exec = self.executor_started.clone();
        let id: &String = &request.get_ref().id.clone();
        let branch = request.get_ref().branch.clone();
        let tree = self.get_tree(id);

        play_execution_at_frame(&tree, request.get_ref().frame);
        if exec.get(id).is_some() {
            return Ok(Response::new(ExecutionStatus{ id: id.clone(), monotonic_counter: 0, branch }));
        }

        // TODO: handle panics in the executor
        let root_tree = self.get_tree("root");
        insert_executor_file_existence_by_id(&root_tree, id.clone());

        create_root_branch(&tree);
        let move_tree = tree.clone();
        let _ = tokio::spawn( async move {
            let mut executor = Executor::new(move_tree);
            executor.run().await;
        });


        let monotonic_counter = get_change_counter_for_branch(&tree, branch);
        exec.insert(id.clone(), true);
        Ok(Response::new(ExecutionStatus{ id: id.clone(), monotonic_counter, branch }))
    }

    #[tracing::instrument]
    async fn pause(&self, request: Request<RequestAtFrame>) -> Result<Response<ExecutionStatus>, Status> {
        debug!("Received pause request: {:?}", request);
        let id = &request.get_ref().id.clone();
        let branch = request.get_ref().branch.clone();
        let tree = self.get_tree(id);
        pause_execution_at_frame(&tree, request.get_ref().frame);
        let monotonic_counter = get_change_counter_for_branch(&tree, branch);
        Ok(Response::new(ExecutionStatus{ id: id.clone(), monotonic_counter, branch}))
    }

    // TODO: branch should target a specific node (via counter and branch)
    #[tracing::instrument]
    async fn branch(&self, request: Request<RequestNewBranch>) -> Result<Response<ExecutionStatus>, Status> {
        debug!("Received branch request: {:?}", request);
        let id = &request.get_ref().id.clone();
        let source_branch_id = request.get_ref().source_branch_id.clone();
        let tree = self.get_tree(id);
        let new_branch_id = create_branch(&tree, source_branch_id, 0);
        let monotonic_counter = get_change_counter_for_branch(&tree, new_branch_id);
        Ok(Response::new(ExecutionStatus{ id: id.clone(), monotonic_counter, branch: new_branch_id}))
    }

    #[tracing::instrument]
    async fn list_branches(&self, request: Request<RequestListBranches>) -> Result<Response<ListBranchesRes>, Status> {
        debug!("Received list_branches request: {:?}", request);
        let id = &request.get_ref().id.clone();
        Ok(Response::new(
            ListBranchesRes {
                id: id.clone(),
                branches: list_branches(&self.get_tree(id)).collect()
            }
        ))

    }

    type ListRegisteredGraphsStream = ReceiverStream<Result<ExecutionStatus, Status>>;

    /// List all of the graphs registered by ID with this execution runtime
    #[tracing::instrument]
    async fn list_registered_graphs(&self, request: tonic::Request<prompt_graph_core::proto2::Empty>) -> Result<Response<Self::ListRegisteredGraphsStream>, Status> {
        debug!("Received list_registered_graphs request: {:?}", request);
        let tree = self.get_tree("root");
        todo!()
    }


    type ListInputProposalsStream = ReceiverStream<Result<InputProposal, Status>>;

    /// Fetch pending input proposals for a given graph. These should be responded to with external
    /// input to the system. Dependent execution nodes will be blocked until these are resolved.
    #[tracing::instrument]
    async fn list_input_proposals(&self, request: Request<RequestOnlyId>) -> Result<Response<Self::ListInputProposalsStream>, Status> {
        debug!("Received list_input_proposals request: {:?}", request);
        let (mut tx, rx) = mpsc::channel(4);
        let tree = self.get_tree(&request.get_ref().id.clone());
        tokio::spawn(async move {
            for prop in scan_all_input_proposals(&tree) {
                tx.send(Ok(prop)).await.unwrap();
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Send a response to an input proposal.
    #[tracing::instrument]
    async fn respond_to_input_proposal(&self, request: Request<RequestInputProposalResponse>) -> Result<Response<Empty>, Status> {
        debug!("Received respond_to_input_proposal request: {:?}", request);
        let tree = self.get_tree(&request.get_ref().id.clone());
        let rec = request.get_ref().clone();
        insert_input_response(&tree, rec);
        Ok(Response::new(Empty::default()))
    }

    type ListChangeEventsStream = ReceiverStream<Result<ChangeValueWithCounter, Status>>;

    /// Fetch the resulting changes from the execution of a graph.
    #[tracing::instrument]
    async fn list_change_events(&self, request: Request<RequestOnlyId>) -> Result<Response<Self::ListChangeEventsStream>, Status> {
        debug!("Received list_change_events request: {:?}", request);
        let (mut tx, rx) = mpsc::channel(4);
        let tree = self.get_tree(&request.get_ref().id.clone());
        tokio::spawn(async move {
            for prop in scan_all_pending_changes(&tree) {
                tx.send(Ok(prop)).await.unwrap();
            }
            for prop in scan_all_resolved_changes(&tree) {
                tx.send(Ok(prop)).await.unwrap();
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type ListNodeWillExecuteEventsStream = ReceiverStream<Result<NodeWillExecuteOnBranch, Status>>;

    async fn list_node_will_execute_events(&self, request: Request<RequestOnlyId>) -> Result<Response<Self::ListNodeWillExecuteEventsStream>, Status> {
        debug!("Received list_node_will_execute_events request: {:?}", request);
        let (mut tx, rx) = mpsc::channel(4);
        let tree = self.get_tree(&request.get_ref().id.clone());
        tokio::spawn(async move {
            for prop in scan_all_will_execute_pending_events(&tree) {
                tx.send(Ok(prop)).await.unwrap();
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn poll_custom_node_will_execute_events(&self, request: Request<FilteredPollNodeWillExecuteEventsRequest>) -> Result<Response<RespondPollNodeWillExecuteEvents>, Status> {
        debug!("Received poll_custom_node_will_execute_events request: {:?}", request);
        let tree = self.get_tree(&request.get_ref().id.clone());

        // Fetch custom node will execute events
        let will_exec_events = scan_all_custom_node_will_execute_events(&tree);
        Ok(Response::new(RespondPollNodeWillExecuteEvents {
            node_will_execute_events: will_exec_events.collect(),
        }))
    }

    // TODO: currently if we ack and then fail, we never progress
    // TODO: in progress nodes must timeout
    async fn ack_node_will_execute_event(&self, request: Request<RequestAckNodeWillExecuteEvent>) -> Result<Response<ExecutionStatus>, Status> {
        debug!("Received ack_node_will_execute_event request: {:?}", request);
        let tree = self.get_tree(&request.get_ref().id.clone());
        let branch = request.get_ref().branch.clone();
        let counter = request.get_ref().counter.clone();
        // this is only used for custom nodes
        move_will_execute_event_to_in_progress(&tree, true, branch, counter);
        Ok(Response::new(ExecutionStatus::default()))
    }

    /// Used to push self-invoke (or other local exec node results) back into the runtime
    #[tracing::instrument]
    async fn push_worker_event(&self, request: Request<FileAddressedChangeValueWithCounter>) -> Result<Response<ExecutionStatus>, Status> {
        debug!("Received push_worker_event request: {:?}", request);
        let tree = self.get_tree(&request.get_ref().id.clone());
        let branch = request.get_ref().branch.clone();
        let counter = request.get_ref().counter.clone();
        let change = request.into_inner().change.expect("Must have a change value");

        let node_will_exec = move_will_execute_event_to_complete(&tree,  true, branch, counter);
        insert_custom_node_execution(&tree, change);
        Ok(Response::new(ExecutionStatus::default()))
    }

    #[tracing::instrument]
    async fn push_template_partial(&self, request: Request<UpsertPromptLibraryRecord>) -> Result<Response<ExecutionStatus>, Status> {
        let tree = self.get_tree(&request.get_ref().id.clone());
        insert_prompt_library_mutation(&tree, request.get_ref());
        Ok(Response::new(ExecutionStatus::default()))
    }
}

#[tokio::main]
pub async fn run_server(url_server: String, file_path: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    // LogTracer::init().unwrap();

    // a builder for `FmtSubscriber`.
    // let subscriber = FmtSubscriber::builder()
    //     // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
    //     // will be written to stdout.
    //     .with_max_level(Level::TRACE)
    //     // completes the builder.
    //     .finish();

    // let (chrome_layer, _guard) = ChromeLayerBuilder::new().build();
    // let (flame_layer, _guard) = FlameLayer::with_file("./tracing.folded").unwrap();
    // tracing_subscriber::registry()
    //     .with(
    //         EnvFilter::from_default_env()
    //             .add_directive("prompt_graph_exec".parse()?)
    //     )
    //     .with(fmt::layer())
    //     .with(chrome_layer)
    //     .with(flame_layer)
    //     .init();


    // Strip protocol from any urls passed in, invalid if URL is passed with protocol
    let url = url_server
        .replace("http://", "")
        .replace("https://", "")
        .replace("localhost", "127.0.0.1");

    let addr = format!("{}", url).parse().unwrap();
    // We create one sled db per execution runtime, and we create a sub-tree for each execution graph
    let server = MyExecutionRuntime::new(file_path);

    println!("ExecutionRuntime listening on {}", addr);

    Server::builder()
        .add_service(ExecutionRuntimeServer::new(server))
        .serve(addr)
        .await?;

    Ok(())
}


#[cfg(test)]
mod tests {
    use prompt_graph_core::templates::render_template_prompt;
    use super::*;

    #[tokio::test]
    async fn test_pushing_a_partial_template() {
        let e = MyExecutionRuntime::new(Some(":memory:".to_string()));
        e.push_template_partial(Request::new(UpsertPromptLibraryRecord {
            description: None,
            template: "Testing".to_string(),
            name: "named".to_string(),
            id: "test".to_string(),
        })).await.unwrap();
        let tree = e.get_tree(&"test".to_string());
    }
}
