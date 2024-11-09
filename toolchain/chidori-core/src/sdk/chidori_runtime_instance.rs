use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use std::fmt;
use uuid::Uuid;
use std::time::Duration;
use anyhow::anyhow;
use dashmap::mapref::one::Ref;
use crate::cells::CellTypes;
use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId};
use crate::execution::execution::execution_state::{EnclosedState};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::identifiers::OperationId;
use crate::execution::primitives::operation::OperationFnOutput;
use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use crate::sdk::interactive_chidori_wrapper::{EventsFromRuntime, SharedState};
use crate::sdk::interactive_chidori_wrapper::CellHolder;
use crate::utils::telemetry::TraceEvents;

/// Instanced environments are not Send and live on a single thread.
/// They execute their operations across multiple threads, but individual OperationNodes
/// must remain on the given thread they're initialized on.
pub struct ChidoriRuntimeInstance {
    pub env_rx: Receiver<UserInteractionMessage>,
    pub db: ExecutionGraph,
    pub execution_head_state_id: ExecutionNodeId,
    pub playback_state: PlaybackState,
    pub runtime_event_sender: Option<Sender<EventsFromRuntime>>,
    pub trace_event_sender: Option<Sender<TraceEvents>>,
    pub shared_state: Arc<Mutex<SharedState>>,
    pub rx_execution_states: TokioReceiver<ExecutionState>,
}

impl std::fmt::Debug for ChidoriRuntimeInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstancedEnvironment")
            .finish()
    }
}

impl ChidoriRuntimeInstance {
    pub fn new() -> ChidoriRuntimeInstance {
        let (tx, rx) = mpsc::channel();
        let mut db = ExecutionGraph::new();
        let execution_event_rx = db.take_execution_event_receiver();
        let state_id = Uuid::nil();
        let playback_state = PlaybackState::Paused;

        ChidoriRuntimeInstance {
            env_rx: rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: None,
            trace_event_sender: None,
            playback_state,
            shared_state: Arc::new(Mutex::new(SharedState::new())),
            rx_execution_states: execution_event_rx,
        }
    }

    // TODO: reload_cells needs to diff the mutations that live on the current branch, with the state
    //       that we see in the shared state when this event is fired.
    pub async fn reload_cells(&mut self) -> anyhow::Result<()> {
        println!("Reloading cells");
        let cells_to_upsert: Vec<_> = {
            let shared_state = self.shared_state.lock().unwrap();
            shared_state.editor_cells.values().map(|cell| cell.clone()).collect()
        };

        // unlock shared_state
        let mut ids = vec![];
        for cell_holder in cells_to_upsert {
            if cell_holder.needs_update {
                ids.push((self.upsert_cell(cell_holder.cell.clone(), cell_holder.op_id).await?, cell_holder));
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
        self.reload_cells().await?;

        let executing_states = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        // Create a channel for error notifications
        let (error_tx, mut error_rx) = tokio::sync::mpsc::channel(32);

        loop {
            // Handle user interactions first for responsiveness
            if let Ok(message) = self.env_rx.try_recv() {
                println!("Received message from user: {:?}", message);
                self.handle_user_interaction_message(message).await?;
            }

            // Check for execution errors
            if let Ok(error) = error_rx.try_recv() {
                // println!("Received execution error: {:?}", error);
                self.set_playback_state(PlaybackState::Paused);
                // TODO: notify the client about the error
                // self.push_update_to_client(&ExecutionState::Error(error));
            }

            // Receives the results of execution during progression of ExecutionStates
            if let Ok(state) = self.rx_execution_states.try_recv() {
                println!("InstancedEnvironment received an execution event {:?}", &state);
                self.push_update_to_client(&state);
                self.set_execution_head(&state);
            }

            {
                if matches!(self.playback_state, PlaybackState::Paused) {
                    continue;
                }
                if matches!(self.playback_state, PlaybackState::Step) {
                    self.set_playback_state(PlaybackState::Paused);
                }
                let execution_head_state_id = self.execution_head_state_id;

                // Acquire lock and check if we're already executing this state
                let mut executing_states_instance = executing_states.lock().await;
                if !executing_states_instance.contains(&execution_head_state_id) {
                    println!("Will eval step, inserting eval state {:?}", &execution_head_state_id);
                    executing_states_instance.insert(execution_head_state_id);
                    drop(executing_states_instance); // Release lock before spawning task

                    // Spawn the progression of the given step in a separate task
                    let executing_states = Arc::clone(&executing_states);
                    let error_tx = error_tx.clone();
                    let state = self.get_state_at_current_execution_head_result()?.clone();
                    tokio::spawn(async move {
                        let result = state.step_execution().await;
                        match result {
                            Ok(_) => {
                                // Handle successful execution
                                executing_states.lock().await.remove(&execution_head_state_id);
                            },
                            Err(err) => {
                                // Ensure we clean up the execution state
                                executing_states.lock().await.remove(&execution_head_state_id);
                                // Send the error through the channel
                                if let Err(send_err) = error_tx.send(err).await {
                                    eprintln!("Failed to send error through channel: {:?}", send_err);
                                }

                            }
                        }
                    });
                }
            }
        }
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
            UserInteractionMessage::SetPlaybackState(state) => {
                self.set_playback_state(state);
            },
            UserInteractionMessage::ReloadCells => {
                self.reload_cells().await?;
            },
            UserInteractionMessage::RevertToState(id) => {
                if let Some(id) = id {
                    self.execution_head_state_id = id;
                    let sender = self.runtime_event_sender.as_mut().unwrap();
                    // let merged_state = self.db.get_merged_state_history(&id);
                    // sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
                    sender.send(EventsFromRuntime::UpdateExecutionHead(id)).unwrap();

                    if let Some(state) = self.db.get_state_at_id(self.execution_head_state_id) {
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
            UserInteractionMessage::MutateCell(cell_holder) => {
                println!("Mutating individual cell");
                let (applied_at, op_id) = self.upsert_cell(cell_holder.cell.clone(), cell_holder.op_id).await?;
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
            UserInteractionMessage::PushChatMessage(msg) => {
                self.db.push_message(msg).await?;
            }
            UserInteractionMessage::RunCellInIsolation(cell, args) => {
                // self.db.execute_operation_in_isolation(&cell.cell, args).await?;
            }
            UserInteractionMessage::Reset => {
                self.db = ExecutionGraph::new();
                self.set_playback_state(PlaybackState::Paused);
                let id = Uuid::nil();
                self.execution_head_state_id = id;
                let mut shared_state = self.shared_state.lock().unwrap();
                shared_state.clear();
            }
        }
        Ok(())
    }

    pub fn get_state_at_current_execution_head_result(&self) -> anyhow::Result<Ref<ExecutionNodeId, ExecutionState>> {
        let state = if let Some(state) = self.db.execution_node_id_to_state.get(&self.execution_head_state_id) {
            state
        } else {
            println!("failed to get state for the target id {:?}", self.execution_head_state_id);
            return Err(anyhow::format_err!("failed to get state for the target id {:?}", self.execution_head_state_id));
        };
        Ok(state)
    }

    #[cfg(test)]
    pub fn get_state_at_current_execution_head(&self) -> ExecutionState {
        self.db.get_state_at_id(self.execution_head_state_id).unwrap()
    }

    fn set_execution_head(&mut self, state: &ExecutionState) {
        println!("Setting execution head");
        // Execution heads can only be Completed states, not states still evaluating
        if matches!(&state.evaluating_enclosed_state, EnclosedState::Close(_)) || (&state).evaluating_enclosed_state == EnclosedState::SelfContained {
            if state.evaluating_fn.is_none() {
                if let Some(sender) = self.runtime_event_sender.as_mut() {
                    sender.send(EventsFromRuntime::UpdateExecutionHead((&state).chronology_id)).unwrap();
                }
                let mut shared_state = self.shared_state.lock().unwrap();
                shared_state.execution_state_head_id = (&state).chronology_id;
                self.execution_head_state_id = (&state).chronology_id;
            }
        }
    }

    fn push_update_to_client(&mut self, state: &ExecutionState) {
        let state_id = state.chronology_id;
        println!("Resulted in state with id {:?}, {:?}", &state_id, &state);
        if let Some(sender) = self.runtime_event_sender.as_mut() {
            sender.send(EventsFromRuntime::DefinitionGraphUpdated(state.get_dependency_graph_flattened())).unwrap();
            let mut cells = vec![];
            for (op_id, cell ) in state.cells_by_id.iter() {
                cells.push(CellHolder {
                    cell: cell.clone(),
                    op_id: op_id.clone(),
                    applied_at: Some(state.chronology_id),
                    needs_update: false,
                });
            }
            sender.send(EventsFromRuntime::ExecutionStateCellsViewUpdated(cells)).unwrap();
            sender.send(EventsFromRuntime::ExecutionGraphUpdated(self.db.get_execution_graph_elements())).unwrap();
            // sender.send(EventsFromRuntime::ExecutionStateChange(self.db.get_merged_state_history(&state_id))).unwrap();
        }
    }

    /// Increment the execution graph by one step
    #[tracing::instrument]
    pub async fn step(&mut self) -> anyhow::Result<Vec<(OperationId, OperationFnOutput)>> {
        let exec_head = self.execution_head_state_id;
        println!("======================= Executing state with id {:?} ======================", &exec_head);
        let (state, outputs) = {
            let state = self.get_state_at_current_execution_head_result()?;
            state.step_execution().await?
        };
        self.push_update_to_client(&state);
        self.set_execution_head(&state);
        Ok(outputs)
    }

    /// Add a cell into the execution graph
    #[tracing::instrument]
    pub async fn upsert_cell(&mut self, cell: CellTypes, op_id: OperationId) -> anyhow::Result<(ExecutionNodeId, OperationId)> {
        let (final_state, op_id2) = {
            let state = self.get_state_at_current_execution_head_result()?;
            let (final_state, op_id2) = state.update_operation(cell, op_id).await?;
            (final_state, op_id2)
        };
        println!("Capturing final_state of the mutate graph operation parent {:?}, id {:?}", final_state.parent_state_chronology_id, final_state.chronology_id);
        let ((state_id, state), op_id) = ((final_state.chronology_id.clone(), final_state.clone()), op_id2);
        self.push_update_to_client(&state);
        self.set_execution_head(&state);
        Ok((state_id, op_id))
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}

#[derive(Debug)]
pub enum UserInteractionMessage {
    SetPlaybackState(PlaybackState),
    RevertToState(Option<ExecutionNodeId>),
    ReloadCells,
    MutateCell(CellHolder),
    Shutdown,
    PushChatMessage(String),
    RunCellInIsolation(CellHolder, RkyvSerializedValue),
    Reset
}




#[derive(PartialEq, Debug, Clone)]
pub enum PlaybackState {
    Paused,
    Step,
    Running,
}