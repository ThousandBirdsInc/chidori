use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc, Mutex};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use std::fmt;
use uuid::Uuid;
use std::time::Duration;
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
    pub fn reload_cells(&mut self) -> anyhow::Result<()> {
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
    pub async fn run(&mut self, initial_playback_state: PlaybackState) -> anyhow::Result<()> {
        println!("Starting instanced environment");
        self.set_playback_state(initial_playback_state);

        // Reload cells to make sure we're up-to-date
        self.reload_cells()?;

        // Channel for receiving step execution results
        let (step_tx, mut step_rx) = tokio::sync::mpsc::channel(100);
        let executing_states = Arc::new(tokio::sync::Mutex::new(HashSet::new()));

        loop {
            // Handle user interactions first for responsiveness
            if let Ok(message) = self.env_rx.try_recv() {
                println!("Received message from user: {:?}", message);
                self.handle_user_interaction_message(message).await?;
            }

            if let Ok(event) = self.execution_event_rx.try_recv() {
                println!("InstancedEnvironment received an execution event {:?}", &event);
                self.handle_execution_event(event).await?;
            }

            let mut should_pause = false;
            {
                let state = self.get_state_at_current_execution_head_result()?;
                let exec_head = self.execution_head_state_id;

                let get_conditional_polling_step = match self.playback_state {
                    PlaybackState::Step => {
                        self.set_playback_state(PlaybackState::Paused);
                        Some((exec_head, ExecutionGraph::immutable_external_step_execution(state)))
                    }
                    PlaybackState::Paused => None,
                    PlaybackState::Running => {
                        Some((exec_head, ExecutionGraph::immutable_external_step_execution(state)))
                    }
                };

                if let Some((executing_from_source_state_id, step)) = get_conditional_polling_step {
                    // Acquire lock and check if we're already executing this state
                    let mut executing_states_instance = executing_states.lock().await;
                    if !executing_states_instance.contains(&executing_from_source_state_id) {
                        println!("Will eval step, inserting eval state {:?}", &executing_from_source_state_id);
                        executing_states_instance.insert(executing_from_source_state_id);
                        drop(executing_states_instance); // Release lock before spawning task

                        // Clone necessary values for the spawned task
                        let step_tx = step_tx.clone();
                        let state_id = executing_from_source_state_id;
                        let executing_states = Arc::clone(&executing_states);

                        // Spawn the step execution in a separate task
                        tokio::spawn(async move {
                            let result = step.await;
                            // Clean up executing state regardless of result
                            // If the result of the execution is a Completion
                            let _ = executing_states.lock().await.remove(&state_id);
                            let _ = step_tx.send((state_id, result)).await;
                        });
                    }
                }
            }

            // Check for completed step results
            match tokio::time::timeout(Duration::from_millis(10), step_rx.recv()).await {
                Ok(Some((resolved_id, step_result))) => {
                    match step_result {
                        Ok((node_id, new_state, outputs)) => {
                            println!("Got result {:?}, {:?}", node_id, new_state);
                            let resulting_state_id = self.db.progress_graph(new_state.clone());
                            self.push_update_to_client(&resulting_state_id, new_state);
                            if outputs.is_empty() {
                                println!("Playback paused, awaiting input from user");
                                should_pause = true;
                            }
                        }
                        Err(_) => {
                            println!("Error should pause");
                            should_pause = true;
                        }
                    }
                }
                Ok(None) => {
                    panic!("Step execution channel closed unexpectedly");
                }
                Err(_) => {}
            }

            if should_pause {
                println!("Setting paused playback state");
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
                if let Some(id) = id {
                    self.execution_head_state_id = id;
                    let merged_state = self.db.get_merged_state_history(&id);
                    let sender = self.runtime_event_sender.as_mut().unwrap();
                    sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
                    sender.send(EventsFromRuntime::UpdateExecutionHead(id)).unwrap();

                    if let Some(ExecutionStateEvaluation::Complete(state)) = self.db.get_state_at_id(self.execution_head_state_id) {
                        let mut cells = vec![];
                        // TODO: keep a separate mapping of cells so we don't need to lock operations
                        for (id, cell) in state.cells_by_id.iter() {
                            cells.push(CellHolder {
                                cell: cell.clone(),
                                op_id: id.clone(),
                                applied_at: None,
                                needs_update: false,
                            });
                        }
                        let mut ss = self.shared_state.lock().unwrap();
                        ss.at_execution_state_cells = cells.clone();
                        sender.send(EventsFromRuntime::ExecutionStateCellsViewUpdated(cells)).unwrap();
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
        if let ExecutionStateEvaluation::Complete(s) = &state {
            if s.stack.is_empty() {
                shared_state.execution_state_head_id = *state_id;
                self.execution_head_state_id = *state_id;
            }
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
        let ((state_id, state), op_id) = self.db.update_operation(self.execution_head_state_id, cell, op_id)?;
        self.push_update_to_client(&state_id, state);
        Ok((state_id, op_id))
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}