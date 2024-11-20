use std::cmp::Ordering;
use std::fmt;
use std::sync::{mpsc, Arc, MutexGuard};

use no_deadlocks::Mutex;
use uuid::Uuid;
use std::sync::mpsc::Sender;
use tracing::dispatcher::DefaultGuard;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use futures_util::future::Shared;
use tracing::{debug, info};
use dashmap::DashMap;
use serde::{Serialize, Serializer};
use serde::ser::SerializeMap;
use std::ops::Deref;
use crate::cells::{BackingFileReference, CellTypes, PlainTextCell};
use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId, MergedStateHistory};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::sdk::chidori_runtime_instance::{ChidoriRuntimeInstance, PlaybackState, UserInteractionMessage};
use crate::sdk::md::{interpret_markdown_code_block, load_folder};
use crate::utils::telemetry::{init_internal_telemetry, TraceEvents};


#[derive(Debug)]
pub struct SharedState {
    pub execution_id_to_evaluation: Arc<DashMap<ExecutionNodeId, ExecutionState>>,
    pub execution_state_head_id: ExecutionNodeId,
}

impl SharedState {
    pub fn new() -> Self {
        SharedState {
            execution_id_to_evaluation: Default::default(),
            execution_state_head_id: Uuid::nil(),
        }
    }

    pub fn clear(&mut self) {
        self.execution_id_to_evaluation = Default::default();
        self.execution_state_head_id = Uuid::nil();
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct CellHolder {
    pub cell: CellTypes,
    pub op_id: OperationId,
    // pub applied_at: Option<ExecutionNodeId>,
    // pub needs_update: bool
}

// Implement Eq to indicate total equality
impl Eq for CellHolder {}



impl PartialOrd for CellHolder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Compare based on the start time from get_ordering_key
        self.get_ordering_key().partial_cmp(&other.get_ordering_key())
    }
}

// You'll also need to implement PartialEq since it's required for PartialOrd
impl PartialEq for CellHolder {
    fn eq(&self, other: &Self) -> bool {
        self.get_ordering_key() == other.get_ordering_key()
    }
}


// Add Ord implementation
impl Ord for CellHolder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Since we're comparing usize values, this will always give us a valid ordering
        self.get_ordering_key().cmp(&other.get_ordering_key())
    }
}

impl CellHolder {
    fn get_ordering_key(&self) -> usize {
        let t = match &self.cell {
            CellTypes::Code(_, t) |
            CellTypes::CodeGen(_, t) |
            CellTypes::Prompt(_, t) |
            CellTypes::PlainText(_, t) |
            CellTypes::Template(_, t) => {
                t
            }
        };
        t.start
    }

    fn from_cell(cell: CellTypes) -> CellHolder {
        CellHolder {
            op_id: Uuid::now_v7(),
            cell,
        }
    }
}

/// InteractiveChidoriWrapper is the high level interface for interacting with our runtime.
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
        // editor_cells: Default::default(),
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
        tracing::subscriber::set_global_default(init_internal_telemetry(sender.clone())).expect("Failed to set global default");
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

    fn load_cells(&mut self, cells: Vec<CellHolder>) -> anyhow::Result<()>  {
        // let cell_name_map = {
        //     let previous_cells = &self.shared_state.lock().unwrap().editor_cells;
        //     previous_cells.values().map(|cell| {
        //         let name = cell.cell.name();
        //         (name.clone(), cell.clone())
        //     }).collect::<HashMap<_, _>>()
        // };
        //
        // let mut new_cells_state = HashMap::new();
        // for cell in cells {
        //     let name = cell.name();
        //     // If the named cell exists in our map already
        //     if let Some(existing_cell_instance) = cell_name_map.get(&name) {
        //         // If it's not the same cell, replace it
        //         if existing_cell_instance.cell != cell {
        //             new_cells_state.insert(existing_cell_instance.op_id, CellHolder {
        //                 cell,
        //                 op_id: existing_cell_instance.op_id,
        //             });
        //         } else {
        //             // It's the same cell so just push our existing state
        //             new_cells_state.insert(existing_cell_instance.op_id, existing_cell_instance.clone());
        //         }
        //     } else {
        //         // This is a new cell, so we push it with a null applied at
        //         let id = Uuid::now_v7();
        //         new_cells_state.insert(id, CellHolder {
        //             cell,
        //             op_id: id,
        //         });
        //     }
        // }
        // debug!("Updating editor cells based on load");
        // self.shared_state.lock().unwrap().editor_cells = new_cells_state;
        self.dispatch_user_interaction_to_instance(UserInteractionMessage::ReloadCells(cells))?;
        Ok(())
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<Vec<CellHolder>> {
        let mut cells = vec![];
        let blocks = crate::sdk::md::extract_blocks(s);
        blocks
            .0
            .iter()
            .filter_map(|block| interpret_markdown_code_block(block, None).unwrap())
            .for_each(|block| { cells.push(block); });
        blocks
            .1
            .iter()
            .filter_map(|block| {
                Some(CellTypes::PlainText( PlainTextCell {
                    backing_file_reference: Some(BackingFileReference {
                        path: "".to_string(),
                        text_range: Some(block.range.clone()),
                    }),
                    text: block.content.clone()
                }, block.range.clone()))
            })
            .for_each(|block| { cells.push(block); });
        cells.sort();
        self.loaded_path = Some("raw_text".to_string());
        let cell_holders: Vec<_> = cells.into_iter().map(|x| CellHolder::from_cell(x)).collect();
        self.load_cells(cell_holders.clone())?;
        Ok(cell_holders)
    }

    pub fn load_md_directory(&mut self, path: &Path) -> anyhow::Result<()> {
        let files = load_folder(path)?;
        let mut cells = vec![];
        for file in files {
            for block in file.code_blocks {
                if let Some(block) = interpret_markdown_code_block(&block, Some(path.to_string_lossy().to_string())).unwrap() {
                    cells.push(block);
                }
            }
        }
        self.loaded_path = Some(path.to_str().unwrap().to_string());
        cells.sort();
        info!("Loading {} cells from {:?}", cells.len(), path);
        self.load_cells(cells.into_iter().map(|x| CellHolder::from_cell(x)).collect())
    }

    pub fn get_instance(&mut self) -> anyhow::Result<ChidoriRuntimeInstance> {
        let (instanced_env_tx, env_rx) = mpsc::channel();
        self.instanced_env_tx = Some(instanced_env_tx);
        let mut db = ExecutionGraph::new();
        let execution_event_rx = db.take_execution_event_receiver();

        let mut shared_state = self.shared_state.lock().unwrap();
        shared_state.execution_id_to_evaluation = db.execution_node_id_to_state.clone();

        Ok(ChidoriRuntimeInstance {
            env_rx,
            db,
            execution_head_state_id: Uuid::nil(),
            runtime_event_sender: self.runtime_event_sender.clone(),
            trace_event_sender: self.trace_event_sender.clone(),
            playback_state: PlaybackState::Paused,
            shared_state: self.shared_state.clone(),
            rx_execution_states: execution_event_rx,
        })
    }
}

#[derive(Clone, Debug)]
pub enum EventsFromRuntime {
    PlaybackState(PlaybackState),
    ExecutionGraphUpdated(Vec<(ExecutionNodeId, ExecutionNodeId)>),
    UpdateExecutionHead(ExecutionNodeId),
    ReceivedChatMessage(String),
}