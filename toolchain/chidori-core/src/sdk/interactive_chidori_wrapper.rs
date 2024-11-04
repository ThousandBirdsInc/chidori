use std::fmt;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use uuid::Uuid;
use std::sync::mpsc::Sender;
use tracing::dispatcher::DefaultGuard;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use futures_util::future::Shared;
use tracing::info;
use dashmap::DashMap;
use serde::{Serialize, Serializer};
use serde::ser::SerializeMap;
use std::ops::Deref;
use crate::cells::{CellTypes};
use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId, MergedStateHistory};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::sdk::chidori_runtime_instance::{ChidoriRuntimeInstance, PlaybackState, UserInteractionMessage};
use crate::sdk::md::{interpret_markdown_code_block, load_folder};
use crate::utils::telemetry::{init_internal_telemetry, TraceEvents};

/// Chidori is the high level interface for interacting with our runtime.
/// It is responsible for loading cells and creating instances of the environment.
/// It is expected to run on a "main thread" while instances may run in background threads.
pub struct InteractiveChidoriWrapper {

    /// Sender to push user requests to the instance, these events result in
    /// state changes within the instance
    pub instanced_env_tx: Option<Sender<UserInteractionMessage>>,

    /// Sender to pass changes in state within instances back to the main thread
    pub runtime_event_sender: Option<Sender<EventsFromRuntime>>,

    /// Sender to collect trace events from instances
    pub trace_event_sender: Option<Sender<TraceEvents>>,

    pub shared_state: Arc<Mutex<SharedState>>,
    pub loaded_path: Option<String>,

    pub tracing_guard: Option<DefaultGuard>
}

impl std::fmt::Debug for InteractiveChidoriWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .finish()
    }
}

fn initialize_shared_state_object() -> Arc<Mutex<SharedState>> {
    Arc::new(Mutex::new(SharedState {
        execution_id_to_evaluation: Default::default(),
        execution_state_head_id: Uuid::nil(),
        editor_cells: Default::default(),
        at_execution_state_cells: vec![],
        latest_state: None,
    }))
}

impl InteractiveChidoriWrapper {
    pub fn new() -> Self {
        InteractiveChidoriWrapper {
            instanced_env_tx: None,
            runtime_event_sender: None,
            trace_event_sender: None,
            loaded_path: None,
            shared_state: initialize_shared_state_object(),
            tracing_guard: None,
        }
    }

    pub fn new_with_events(sender: Sender<TraceEvents>, runtime_event_sender: Sender<EventsFromRuntime>) -> Self {
        let init_telemetry = init_internal_telemetry(sender.clone());
        // tracing::subscriber::set_global_default(init_telemetry.clone()).expect("Failed to set global default");
        let guard: DefaultGuard = tracing::subscriber::set_default(init_telemetry);
        InteractiveChidoriWrapper {
            instanced_env_tx: None,
            runtime_event_sender: Some(runtime_event_sender),
            trace_event_sender: Some(sender),
            loaded_path: None,
            shared_state: initialize_shared_state_object(),
            tracing_guard: Some(guard)
        }
    }

    #[tracing::instrument]
    pub fn dispatch_user_interaction_to_instance(&self, action: UserInteractionMessage) -> anyhow::Result<()> {
        if let Some(tx) = &self.instanced_env_tx {
            tx.send(action)?;
        }
        Ok(())
    }

    fn load_cells(&mut self, cells: Vec<CellTypes>) -> anyhow::Result<()>  {
        // TODO: this overrides the entire shared state object
        let cell_name_map = {
            let previous_cells = &self.shared_state.lock().unwrap().editor_cells;
            previous_cells.values().map(|cell| {
                let name = cell.cell.name();
                (name.clone(), cell.clone())
            }).collect::<HashMap<_, _>>()
        };

        let mut new_cells_state = HashMap::new();
        for cell in cells {
            let name = cell.name();
            // If the named cell exists in our map already
            if let Some(existing_cell_instance) = cell_name_map.get(&name) {
                // If it's not the same cell, replace it
                if existing_cell_instance.cell != cell {
                    new_cells_state.insert(existing_cell_instance.op_id, CellHolder {
                        cell,
                        applied_at: None,
                        op_id: existing_cell_instance.op_id,
                        needs_update: true
                    });
                } else {
                    // It's the same cell so just push our existing state
                    new_cells_state.insert(existing_cell_instance.op_id, existing_cell_instance.clone());
                }
            } else {
                // This is a new cell, so we push it with a null applied at
                let id = Uuid::now_v7();
                new_cells_state.insert(id, CellHolder {
                    cell,
                    applied_at: None,
                    op_id: id,
                    needs_update: true
                });
            }
        }
        self.shared_state.lock().unwrap().editor_cells = new_cells_state;
        println!("Cells commit to shared state");
        self.dispatch_user_interaction_to_instance(UserInteractionMessage::ReloadCells)?;
        Ok(())
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<()> {
        let mut cells = vec![];
        crate::sdk::md::extract_code_blocks(s)
            .iter()
            .filter_map(|block| interpret_markdown_code_block(block, None).unwrap())
            .for_each(|block| { cells.push(block); });
        cells.sort();
        self.loaded_path = Some("raw_text".to_string());
        self.load_cells(cells)
    }

    pub fn load_md_directory(&mut self, path: &Path) -> anyhow::Result<()> {
        let files = load_folder(path)?;
        let mut cells = vec![];
        for file in files {
            for block in file.result {
                if let Some(block) = interpret_markdown_code_block(&block, Some(path.to_string_lossy().to_string())).unwrap() {
                    cells.push(block);
                }
            }
        }
        self.loaded_path = Some(path.to_str().unwrap().to_string());
        cells.sort();
        info!("Loading {} cells from {:?}", cells.len(), path);
        self.load_cells(cells)
    }

    pub fn get_instance(&mut self) -> anyhow::Result<ChidoriRuntimeInstance> {
        let (instanced_env_tx, env_rx) = mpsc::channel();
        self.instanced_env_tx = Some(instanced_env_tx);
        let mut db = ExecutionGraph::new();
        let execution_event_rx = db.take_execution_event_receiver();
        let state_id = Uuid::nil();
        let playback_state = PlaybackState::Paused;

        let mut shared_state = self.shared_state.lock().unwrap();
        shared_state.execution_id_to_evaluation = db.execution_node_id_to_state.clone();

        Ok(ChidoriRuntimeInstance {
            env_rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: self.runtime_event_sender.clone(),
            trace_event_sender: self.trace_event_sender.clone(),
            playback_state,
            shared_state: self.shared_state.clone(),
            rx_execution_states: execution_event_rx,
        })
    }
}

#[derive(Clone, Debug)]
pub enum EventsFromRuntime {
    PlaybackState(PlaybackState),
    DefinitionGraphUpdated(Vec<(OperationId, OperationId, Vec<DependencyReference>)>),
    ExecutionGraphUpdated(Vec<(ExecutionNodeId, ExecutionNodeId)>),
    ExecutionStateChange(MergedStateHistory),
    EditorCellsUpdated(HashMap<OperationId, CellHolder>),
    StateAtId(ExecutionNodeId, ExecutionState),
    UpdateExecutionHead(ExecutionNodeId),
    ReceivedChatMessage(String),
    ExecutionStateCellsViewUpdated(Vec<CellHolder>),
}

#[derive(Debug)]
pub struct SharedState {
    pub execution_id_to_evaluation: Arc<DashMap<ExecutionNodeId, ExecutionState>>,
    pub execution_state_head_id: ExecutionNodeId,
    pub latest_state: Option<ExecutionState>,
    pub editor_cells: HashMap<OperationId, CellHolder>,
    pub at_execution_state_cells: Vec<CellHolder>,
}

impl Serialize for SharedState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        let mut state = serializer.serialize_map(None)?;
        if let Some(map) = &self.latest_state {
            for (k, v) in &map.state {
                state.serialize_entry(&k, &v.deref().output)?; // Dereference `Arc` to serialize the value inside
            }
        }
        state.end()
    }
}

impl SharedState {
    pub fn new() -> Self {
        SharedState {
            execution_id_to_evaluation: Default::default(),
            execution_state_head_id: Uuid::nil(),
            latest_state: None,
            editor_cells: Default::default(),
            at_execution_state_cells: vec![],
        }
    }

    pub fn clear(&mut self) {
        self.execution_id_to_evaluation = Default::default();
        self.execution_state_head_id = Uuid::nil();
        self.latest_state = None;
        self.editor_cells = Default::default();
        self.at_execution_state_cells = vec![];
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct CellHolder {
    pub cell: CellTypes,
    pub op_id: OperationId,
    pub applied_at: Option<ExecutionNodeId>,
    pub needs_update: bool
}