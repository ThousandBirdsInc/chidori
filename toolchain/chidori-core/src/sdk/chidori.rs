use std::fmt;
use std::sync::{Arc, mpsc, Mutex, MutexGuard};
use uuid::Uuid;
use std::sync::mpsc::Sender;
use tracing::dispatcher::DefaultGuard;
use std::collections::HashMap;
use std::path::Path;
use crate::cells::{CellTypes, get_cell_name};
use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::sdk::entry::{CellHolder, EventsFromRuntime, PlaybackState, SharedState, UserInteractionMessage};
use crate::sdk::instanced_environment::InstancedEnvironment;
use crate::sdk::md::{interpret_markdown_code_block, load_folder};
use crate::utils::telemetry::{init_internal_telemetry, TraceEvents};

/// Chidori is the high level interface for interacting with our runtime.
/// It is responsible for loading cells and creating instances of the environment.
/// It is expected to run on a "main thread" while instances may run in background threads.
pub struct Chidori {

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

impl std::fmt::Debug for Chidori {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .finish()
    }
}

impl Chidori {
    pub fn new() -> Self {
        Chidori {
            instanced_env_tx: None,
            runtime_event_sender: None,
            trace_event_sender: None,
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                execution_id_to_evaluation: Default::default(),
                execution_state_head_id: Uuid::nil(),
                editor_cells: Default::default(),
                at_execution_state_cells: vec![],
                latest_state: None,
            })),
            tracing_guard: None,
        }
    }

    pub fn new_with_events(sender: Sender<TraceEvents>, runtime_event_sender: Sender<EventsFromRuntime>) -> Self {
        tracing::subscriber::set_global_default(init_internal_telemetry(sender.clone())).expect("Failed to set global default");
        let guard: DefaultGuard = tracing::subscriber::set_default(init_internal_telemetry(sender.clone()));
        Chidori {
            instanced_env_tx: None,
            runtime_event_sender: Some(runtime_event_sender),
            trace_event_sender: Some(sender),
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                execution_id_to_evaluation: Default::default(),
                execution_state_head_id: Uuid::nil(),
                editor_cells: Default::default(),

                at_execution_state_cells: vec![],

                latest_state: None,
            })),
            tracing_guard: Some(guard)
        }
    }

    pub fn get_shared_state(&self) -> MutexGuard<'_, SharedState> {
        self.shared_state.lock().unwrap()
    }

    pub fn get_cells(&self) -> Vec<CellTypes> {
        vec![]
    }

    #[tracing::instrument]
    pub fn handle_user_action(&self, action: UserInteractionMessage) -> anyhow::Result<()> {
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
                let name = get_cell_name(&cell.cell);
                (name.clone(), cell.clone())
            }).collect::<HashMap<_, _>>()
        };

        let mut new_cells_state = HashMap::new();
        for cell in cells {
            let name = get_cell_name(&cell);
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
                let id = Uuid::new_v4();
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
        self.handle_user_action(UserInteractionMessage::ReloadCells)?;
        Ok(())
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<()> {
        let mut cells = vec![];
        crate::sdk::md::extract_code_blocks(s)
            .iter()
            .filter_map(|block| interpret_markdown_code_block(block).unwrap())
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
                if let Some(block) = interpret_markdown_code_block(&block).unwrap() {
                    cells.push(block);
                }
            }
        }
        self.loaded_path = Some(path.to_str().unwrap().to_string());
        cells.sort();
        self.load_cells(cells)
    }

    pub fn get_instance(&mut self) -> anyhow::Result<InstancedEnvironment> {
        let (instanced_env_tx, env_rx) = mpsc::channel();
        self.instanced_env_tx = Some(instanced_env_tx);
        let mut db = ExecutionGraph::new();
        let execution_event_rx = db.take_execution_event_receiver();
        let state_id = Uuid::nil();
        let playback_state = PlaybackState::Paused;

        let mut shared_state = self.shared_state.lock().unwrap();
        shared_state.execution_id_to_evaluation = db.execution_node_id_to_state.clone();

        Ok(InstancedEnvironment {
            env_rx,
            db,
            execution_head_state_id: state_id,
            runtime_event_sender: self.runtime_event_sender.clone(),
            trace_event_sender: self.trace_event_sender.clone(),
            playback_state,
            shared_state: self.shared_state.clone(),
            execution_event_rx,
        })
    }
}