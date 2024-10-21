use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc, Mutex};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use std::fmt;
use uuid::Uuid;
use dashmap::DashMap;
use std::pin::Pin;
use std::time::Duration;
use futures_util::future::select_all;
use futures_util::FutureExt;
use crate::cells::CellTypes;
use crate::execution::execution::execution_graph::{ExecutionEvent, ExecutionGraph, ExecutionNodeId};
use crate::execution::execution::execution_state::ExecutionStateEvaluation;
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::identifiers::OperationId;
use crate::execution::primitives::operation::OperationFnOutput;
use crate::sdk::entry::{CellHolder, EventsFromRuntime, PlaybackState, SharedState, UserInteractionMessage};
use crate::utils::telemetry::TraceEvents;

/// Instanced environments are not Send and live on a single thread.
/// They execute their operations across multiple threads, but individual OperationNodes
/// must remain on the given thread they're initialized on.
pub struct InstancedEnvironment {
    pub env_rx: Receiver<UserInteractionMessage>,
    pub db: ExecutionGraph,
    pub execution_head_state_id: ExecutionNodeId,
    pub playback_state: PlaybackState,
    pub runtime_event_sender: Option<Sender<EventsFromRuntime>>,
    pub trace_event_sender: Option<Sender<TraceEvents>>,
    pub shared_state: Arc<Mutex<SharedState>>,
    pub execution_event_rx: TokioReceiver<ExecutionEvent>,
}

impl std::fmt::Debug for InstancedEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstancedEnvironment")
            .finish()
    }
}

impl InstancedEnvironment {
    pub(crate) fn new() -> InstancedEnvironment {
        let (tx, rx) = mpsc::channel();
        let mut db = ExecutionGraph::new();
        let execution_event_rx = db.take_execution_event_receiver();
        let state_id = Uuid::nil();
        let playback_state = PlaybackState::Paused;

        InstancedEnvironment {
            env_rx: rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: None,
            trace_event_sender: None,
            playback_state,
            shared_state: Arc::new(Mutex::new(SharedState::new())),
            execution_event_rx,
        }
    }

    // TODO: reload_cells needs to diff the mutations that live on the current branch, with the state
    //       that we see in the shared state when this event is fired.
    pub(crate) fn reload_cells(&mut self) -> anyhow::Result<()> {
        println!("Reloading cells");
        let cells_to_upsert: Vec<_> = {
            let shared_state = self.shared_state.lock().unwrap();
            shared_state.editor_cells.values().map(|cell| cell.clone()).collect()
        };

        // unlock shared_state
        let mut ids = vec![];
        for cell_holder in cells_to_upsert {
            if cell_holder.needs_update {
                ids.push((self.upsert_cell(cell_holder.cell.clone(), cell_holder.op_id)?, cell_holder));
            } else {
                // TODO: remove these unwraps and handle this better
                ids.push(((cell_holder.applied_at.unwrap(), cell_holder.op_id), cell_holder));
            }
        }

        // lock again and update
        let mut shared_state = self.shared_state.lock().unwrap();
        for ((applied_at, op_id), cell_holder) in ids {
            shared_state.editor_cells.insert(op_id, cell_holder);
            shared_state.editor_cells.entry(op_id).and_modify(|cell| {
                cell.applied_at = Some(applied_at.clone());
                cell.op_id = op_id;
                cell.needs_update = false;
            });
        }

        if let Some(sender) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::EditorCellsUpdated(shared_state.editor_cells.clone())).unwrap();
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        println!("Shutting down Chidori runtime.");
        self.db.shutdown().await;
    }


    // #[tracing::instrument]
    pub async fn wait_until_ready(&mut self) -> anyhow::Result<()> {
        println!("Awaiting initialization of the execution coordinator");
        self.db.execution_depth_orchestration_initialized_notify.notified().await;
        Ok(())
    }



    /// Entrypoint for execution of an instanced environment, handles messages from the host
    // #[tracing::instrument]
    pub async fn run(&mut self) -> anyhow::Result<()> {
        println!("Starting instanced environment");
        self.set_playback_state(PlaybackState::Paused);

        // Reload cells to make sure we're up-to-date
        self.reload_cells()?;

        // Get the current span ID
        // let current_span_id = Span::current().id().expect("There is no current span");

        let mut executing_states = DashMap::new();
        let mut loop_remaining_futures = vec![];
        loop {
            // println!("Looping UserInteraction");
            // let closure_span = tracing::span!(parent: &current_span_id, tracing::Level::INFO, "execution_instance_loop");
            // let _enter = closure_span.enter();

            // Handle user interactions first for responsiveness
            if let Ok(message) = self.env_rx.try_recv() {
                println!("Received message from user: {:?}", message);
                self.handle_user_interaction_message(message).await?;
            }


            tokio::select! {
                Some(event) = self.execution_event_rx.recv() => {
                    println!("InstancedEnvironment received an execution event {:?}", &event);
                    self.handle_execution_event(event).await?;
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                }
            }

            // TODO: what if we're somehow contending over resources, and that's why the fast polling makes this fail

            let mut should_pause = false;;
            let mut timeout_future_available = false;
            {
                let state = self.get_state_at_current_execution_head_result()?;
                let exec_head = self.execution_head_state_id;
                let get_conditional_polling_step = match self.playback_state {
                    PlaybackState::Step => {
                        self.set_playback_state(PlaybackState::Paused);
                        Some((exec_head, ExecutionGraph::immutable_external_step_execution(state)))
                    }
                    PlaybackState::Paused => {
                        None
                    }
                    PlaybackState::Running => {
                        Some((exec_head, ExecutionGraph::immutable_external_step_execution(state)))
                    }
                };

                if let Some((executing_from_source_state_id, step)) = get_conditional_polling_step {

                    let mut futures_vec: Vec<
                        Pin<Box<dyn futures::Future<Output = Option<anyhow::Result<(ExecutionNodeId, ExecutionStateEvaluation, Vec<(Uuid, OperationFnOutput)>)>>> + Send>>,
                    > = loop_remaining_futures;

                    // Only add a step iteration if we're not currently executing the given state
                    if !executing_states.contains_key(&executing_from_source_state_id) {
                        println!("Will eval step, inserting eval state {:?}", &executing_from_source_state_id);
                        executing_states.insert(executing_from_source_state_id, true);
                        let fut_with_some = step.map(move |res| Some(res)).boxed();
                        futures_vec.push(fut_with_some);
                    }

                    // TODO: when we decrease the timeout of the sleep, we start to fail to progress execution
                    // This is intended to be 10ms but due to the above bug, we're set to 2000ms
                    // My current theory is that looping more often causes contention over
                    // some resource that causes evaluation to slow to a halt
                    // Add a 2000ms timeout future
                    if !timeout_future_available {
                        let timeout_future = tokio::time::sleep(Duration::from_millis(2000)).then(|_| {
                            // Return a special result to indicate timeout
                            futures::future::ready(None)
                        });
                        futures_vec.push(timeout_future.boxed());
                        timeout_future_available = true;
                    }

                    let futures_vec_len = futures_vec.len();

                    let (completed_result, _completed_index, remaining_futures) = select_all(futures_vec).await;
                    println!("completed_result {:?}", completed_result);
                    loop_remaining_futures = remaining_futures;
                    println!("loop_remaining_futures count {:?}", loop_remaining_futures.len());

                    match completed_result {
                        Some(anyhow::Result::Ok((resolved_id, new_state, o)))  => {
                            println!("Got completed result {:?}", &resolved_id);

                            // TODO: remove from the dashmap when we see the parent_id here
                            match &new_state {
                                ExecutionStateEvaluation::Error(s) => {
                                    println!("Got result {:?}, {:?}", resolved_id, new_state);
                                    executing_states.remove(&resolved_id);
                                }
                                ExecutionStateEvaluation::EvalFailure(_) => {}
                                ExecutionStateEvaluation::Complete(s) => {
                                    println!("Got result {:?}, {:?}", resolved_id, new_state);
                                    executing_states.remove(&resolved_id);
                                }
                                ExecutionStateEvaluation::Executing(s) => {
                                    println!("Still executing result {:?}, {:?}", resolved_id, new_state);
                                }
                            }
                            let resulting_state_id = self.db.progress_graph(new_state.clone());
                            self.push_update_to_client(&resulting_state_id, new_state);
                            if o.is_empty() {
                                println!("Playback paused, awaiting input from user");
                                should_pause = true;
                            }
                        }
                        Some(anyhow::Result::Err(_))  => {
                            println!("Error should pause");
                            should_pause = true;
                        }
                        None => {
                            timeout_future_available = false;
                            println!("Loop timeout, continuing");
                        }
                    }
                }
            }
            if should_pause {
                self.set_playback_state(PlaybackState::Paused);
            }
        }
        unreachable!("We've exited the run loop");
        Ok(())
    }

    fn set_playback_state(&mut self, playback_state: PlaybackState) {
        self.playback_state = playback_state.clone();
        if let Some(sender ) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::PlaybackState(playback_state)).unwrap();
        }
    }

    async fn handle_user_interaction_message(&mut self, message: UserInteractionMessage) -> Result<(), anyhow::Error> {
        println!("Received user interaction message");
        match message {
            UserInteractionMessage::Step => {
                self.set_playback_state(PlaybackState::Step);
            },
            UserInteractionMessage::Play => {
                // self.get_state_at_current_execution_head().render_dependency_graph();
                self.set_playback_state(PlaybackState::Running);
            },
            UserInteractionMessage::Pause => {
                // self.get_state_at_current_execution_head().render_dependency_graph();
                self.set_playback_state(PlaybackState::Paused);
            },
            UserInteractionMessage::ReloadCells => {
                self.reload_cells()?;
            },
            UserInteractionMessage::FetchStateAt(id) => {
                let state = self.get_state_at(id);
                let sender = self.runtime_event_sender.as_mut().unwrap();
                sender.send(EventsFromRuntime::StateAtId(id, state)).unwrap();
            },
            UserInteractionMessage::RevertToState(id) => {
                println!("=== 0");
                if let Some(id) = id {
                    self.execution_head_state_id = id;
                    println!("=== A");
                    let merged_state = self.db.get_merged_state_history(&id);
                    let sender = self.runtime_event_sender.as_mut().unwrap();
                    sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
                    sender.send(EventsFromRuntime::UpdateExecutionHead(id)).unwrap();
                    println!("=== B");

                    if let Some(ExecutionStateEvaluation::Complete(state)) = self.db.get_state_at_id(self.execution_head_state_id) {
                        let mut cells = vec![];
                        // TODO: keep a separate mapping of cells so we don't need to lock operations
                        println!("=== C");
                        for (id, cell) in state.cells_by_id.iter() {
                            cells.push(CellHolder {
                                cell: cell.clone(),
                                op_id: id.clone(),
                                applied_at: None,
                                needs_update: false,
                            });
                        }
                        println!("=== D");
                        let mut ss = self.shared_state.lock().unwrap();
                        ss.at_execution_state_cells = cells.clone();
                        sender.send(EventsFromRuntime::ExecutionStateCellsViewUpdated(cells)).unwrap();
                        println!("=== E");
                    }
                }
            },
            UserInteractionMessage::Shutdown => {
                self.shutdown().await;
            }
            UserInteractionMessage::UserAction(_) => {}
            UserInteractionMessage::FetchCells => {}
            UserInteractionMessage::MutateCell(cell_holder) => {
                println!("Mutating individual cell");
                let (applied_at, op_id) = self.upsert_cell(cell_holder.cell.clone(), cell_holder.op_id)?;
                let mut shared_state = self.shared_state.lock().unwrap();
                shared_state.editor_cells.insert(op_id, cell_holder);
                shared_state.editor_cells.entry(op_id).and_modify(|cell| {
                    cell.applied_at = Some(applied_at.clone());
                    cell.op_id = op_id;
                    cell.needs_update = false;
                });
                if let Some(sender) = self.runtime_event_sender.as_mut() {
                    sender.send(EventsFromRuntime::EditorCellsUpdated(shared_state.editor_cells.clone())).unwrap();
                }
            }
            UserInteractionMessage::ChatMessage(msg) => {
                self.db.push_message(msg).await?;
            }
            UserInteractionMessage::RunCellInIsolation(cell, args) => {
                // self.db.execute_operation_in_isolation(&cell.cell, args).await?;
            }
        }
        Ok(())
    }

    async fn handle_execution_event(&mut self, event: ExecutionEvent) -> anyhow::Result<()> {
        let ExecutionEvent { id, evaluation } = event;
        self.push_update_to_client(&id, evaluation);
        Ok(())
    }

    pub fn get_state_at(&self, id: ExecutionNodeId) -> ExecutionState {
        match self.db.get_state_at_id(id).unwrap() {
            ExecutionStateEvaluation::Complete(s) => s,
            ExecutionStateEvaluation::Executing(s) => s,
            ExecutionStateEvaluation::Error(s) => s,
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        }
    }

    pub fn get_state_at_current_execution_head_result(&self) -> anyhow::Result<ExecutionStateEvaluation> {
        let state = if let Some(state) = self.db.get_state_at_id(self.execution_head_state_id) { state } else {
            println!("failed to get state for the target id {:?}", self.execution_head_state_id);
            return Err(anyhow::format_err!("failed to get state for the target id {:?}", self.execution_head_state_id));
        };
        Ok(state)
    }

    pub fn get_state_at_current_execution_head(&self) -> ExecutionState {
        match self.db.get_state_at_id(self.execution_head_state_id).unwrap() {
            ExecutionStateEvaluation::Complete(s) => s,
            ExecutionStateEvaluation::Executing(s) => s,
            ExecutionStateEvaluation::Error(s) => s,
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        }
    }

    fn push_update_to_client(&mut self, state_id: &ExecutionNodeId, state: ExecutionStateEvaluation) {
        println!("Resulted in state with id {:?}, {:?}", &state_id, &state);
        if let Some(sender) = self.runtime_event_sender.as_mut() {
            if let ExecutionStateEvaluation::Complete(s) = &state {
                sender.send(EventsFromRuntime::DefinitionGraphUpdated(s.get_dependency_graph_flattened())).unwrap();
                let mut cells = vec![];
                for (op_id, cell ) in s.cells_by_id.iter() {
                    cells.push(CellHolder {
                        cell: cell.clone(),
                        op_id: op_id.clone(),
                        applied_at: Some(s.id),
                        needs_update: false,
                    });
                }
                sender.send(EventsFromRuntime::ExecutionStateCellsViewUpdated(cells)).unwrap();
            }
            sender.send(EventsFromRuntime::ExecutionGraphUpdated(self.db.get_execution_graph_elements())).unwrap();
            sender.send(EventsFromRuntime::ExecutionStateChange(self.db.get_merged_state_history(&state_id))).unwrap();
            sender.send(EventsFromRuntime::UpdateExecutionHead(*state_id)).unwrap();
        }

        let mut shared_state = self.shared_state.lock().unwrap();
        // Only completed states update execution heads
        if let ExecutionStateEvaluation::Complete(_) = &state {
            shared_state.execution_state_head_id = *state_id;
            self.execution_head_state_id = *state_id;
        }
        shared_state.execution_id_to_evaluation
            .entry(*state_id)
            .and_modify(|existing_state| {
                if !matches!(existing_state, ExecutionStateEvaluation::Complete(_)) {
                    *existing_state = state.clone();
                }
            })
            .or_insert(state);
    }

    /// Increment the execution graph by one step
    #[tracing::instrument]
    pub(crate) async fn step(&mut self) -> anyhow::Result<Vec<(OperationId, OperationFnOutput)>> {
        let exec_head = self.execution_head_state_id;
        println!("======================= Executing state with id {:?} ======================", &exec_head);
        let state = self.get_state_at_current_execution_head_result()?;
        let (state_id, state, outputs) = ExecutionGraph::immutable_external_step_execution(state).await?;
        self.push_update_to_client(&state_id, state);
        Ok(outputs)
    }



    /// Add a cell into the execution graph
    #[tracing::instrument]
    pub fn upsert_cell(&mut self, cell: CellTypes, op_id: OperationId) -> anyhow::Result<(ExecutionNodeId, OperationId)> {
        println!("Upserting cell into state with id {:?}", &self.execution_head_state_id);
        let ((state_id, state), op_id) = self.db.mutate_graph(self.execution_head_state_id, cell, op_id)?;
        self.push_update_to_client(&state_id, state);
        Ok((state_id, op_id))
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}