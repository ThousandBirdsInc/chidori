use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bevy_utils::tracing::{error, info};
use chidori_core::uuid::Uuid;
use petgraph::graph::NodeIndex;
use petgraph::prelude::StableGraph;
use notify_debouncer_full::{
    DebounceEventResult,
    new_debouncer, 
    notify::{RecursiveMode, Watcher},
};

use chidori_core::execution::execution::execution_graph::ExecutionNodeId;
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::identifiers::OperationId;
use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
use chidori_core::sdk::chidori_runtime_instance::{PlaybackState, UserInteractionMessage};
use chidori_core::cells::TextRange;
use chidori_core::sdk::md::cell_type_to_markdown;

use super::types::{ChidoriState, CellState};

fn hash_graph(input: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

impl ChidoriState {
    pub fn construct_stablegraph_from_chidori_execution_graph(&self, execution_graph: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> (StableGraph<ExecutionNodeId, ()>, HashMap<ExecutionNodeId, NodeIndex>) {
        // TODO: cache this
        let mut dataset = StableGraph::new();
        let mut node_ids = HashMap::new();
        for (a, b) in execution_graph {
            let node_index_a = *node_ids
                .entry(a.clone())
                .or_insert_with(|| dataset.add_node(a.clone()));
            let node_index_b = *node_ids
                .entry(b.clone())
                .or_insert_with(|| dataset.add_node(b.clone()));
            dataset.add_edge(node_index_a, node_index_b, ());
        }
        (dataset, node_ids)
    }

    /// Check if the target ExecutionNodeId, traversing back from the current execution head is included
    pub fn exists_in_current_tree(&self, n: &ExecutionNodeId) -> bool {
        let h = self.current_execution_head;
        let (graph, nodes) = self.construct_stablegraph_from_chidori_execution_graph(&self.execution_graph);
        if let Some(h_idx) = nodes.get(&h) {
            let mut current = *h_idx;
            let mut current_weight = graph.node_weight(current);
            while current_weight != Some(&Uuid::nil()) {
                current_weight = graph.node_weight(current);
                if current_weight == Some(n) {
                    return true;
                }
                // Get the parent of the current node
                if let Some(parent) = graph.neighbors_directed(current, petgraph::Direction::Incoming).next() {
                    current = parent;
                } else {
                    // If there's no parent, we've reached the root
                    break;
                }
            }
        }

        false
    }

    pub fn get_loaded_path(&self) -> String {
        let env = self.chidori.lock().unwrap();
        if env.loaded_path.is_none() {
            return "".to_string();
        }
        env.loaded_path.as_ref().unwrap().to_string()
    }

    #[cfg(test)]
    pub fn set_execution_state_at_id(
        &self,
        execution_node_id: &ExecutionNodeId,
        execution_state: ExecutionState
    ) {
        let chidori = self.chidori.lock().unwrap();
        {
            let shared_state = chidori.shared_state.lock().unwrap();
            let exec = shared_state.execution_id_to_evaluation.clone();
            exec.insert(*execution_node_id, execution_state);
        };
    }

    pub fn get_execution_state_at_id(
        &self,
        execution_node_id: &ExecutionNodeId,
    ) -> Option<ExecutionState> {
        // TODO: this is 3 locks just to get the current state (which is bad)
        let chidori = self.chidori.lock().unwrap();
        let eval = {
            let shared_state = chidori.shared_state.lock().unwrap();
            let exec = shared_state.execution_id_to_evaluation.clone();
            let eval = exec.get(&execution_node_id).map(|x| x.clone());
            eval
        };
        eval
    }

    pub fn step(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.dispatch_user_interaction_to_instance(UserInteractionMessage::SetPlaybackState(PlaybackState::Step))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn play(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.dispatch_user_interaction_to_instance(UserInteractionMessage::SetPlaybackState(PlaybackState::Running))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pause(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.dispatch_user_interaction_to_instance(UserInteractionMessage::SetPlaybackState(PlaybackState::Paused))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_execution_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.dispatch_user_interaction_to_instance(UserInteractionMessage::RevertToState(Some(id)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn reset(&mut self) -> anyhow::Result<(), String> {
        // TODO: this does not clear the state of the visualized execution graph fully
        let env = self.chidori.lock().unwrap();
        env.dispatch_user_interaction_to_instance(UserInteractionMessage::Reset)
            .map_err(|e| e.to_string())?;
        self.watched_path = std::sync::Mutex::new(None);
        self.background_thread = std::sync::Mutex::new(None);
        self.file_watch = std::sync::Mutex::new(None);
        self.application_state_is_displaying_example_modal = true;
        self.current_playback_state = PlaybackState::Paused;
        self.local_cell_state = Default::default();
        self.log_messages = vec![];
        self.definition_graph = vec![];
        self.execution_graph = vec![];
        self.grouped_nodes = Default::default();
        self.current_execution_head = Default::default();
        self.trace_events = vec![];
        Ok(())
    }

    pub fn save_notebook(&mut self) {
        // Collect unique file paths and their modifications
        let mut file_modifications: HashMap<String, Vec<(TextRange, String)>> = HashMap::new();
        
        // Gather modifications from dirty cells
        for cell in self.local_cell_state.iter() {
            let (_, x) = cell.pair();
            if let Ok(x) = x.lock() {
                if let Some(cell) = &x.cell {
                    if cell.is_dirty_editor {
                        if let Some(bfr) = cell.cell.backing_file_reference() {
                            if let Some(text_range) = &bfr.text_range {
                                // Ensure the new content ends with a newline if original did
                                let body = cell_type_to_markdown(&cell.cell);
                                let mut new_content = body.trim_end().to_string();
                                if body.ends_with('\n') {
                                    new_content.push('\n');
                                }

                                file_modifications
                                    .entry(bfr.path.clone())
                                    .or_default()
                                    .push((text_range.clone(), new_content));
                            }
                        }
                    }
                }
            }
        }

        // Apply modifications to each file
        for (path, modifications) in file_modifications {
            if let Ok(original_content) = std::fs::read_to_string(&path) {
                let mut content = original_content.clone();
                
                // Sort modifications by start position in reverse order
                let mut mods = modifications;
                mods.sort_by(|a, b| b.0.start.cmp(&a.0.start));

                // Apply each modification
                for (range, new_text) in mods {
                    if range.start <= content.len() && range.end <= content.len() {
                        content.replace_range(range.start..range.end, &new_text);
                    }
                }

                // Only write if content has actually changed
                if content != original_content {
                    if let Err(e) = std::fs::write(&path, content) {
                        error!("Failed to write to file {}: {}", path, e);
                    }
                }
            }
        }
    }

    pub fn update_cell(&self, cell_holder: CellHolder) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        {
            let chidori_guard = chidori.lock().expect("Failed to lock chidori");
            chidori_guard.dispatch_user_interaction_to_instance(UserInteractionMessage::MutateCell(cell_holder))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn load_string(&mut self, file_content: &str) -> anyhow::Result<(), String> {
        use std::sync::Arc;
        self.application_state_is_displaying_example_modal = false;
        let chidori = self.chidori.clone();
        let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
        let cell_holders = chidori_guard.load_md_string(file_content).expect("Failed to load markdown string");
        for cell in cell_holders {
            self.local_cell_state.insert(cell.op_id, Arc::new(std::sync::Mutex::new(CellState {
                cell: Some(cell),
                ..Default::default()
            })));
        }
        Ok(())
    }

    pub fn load_and_watch_directory(&self, path: String) -> anyhow::Result<(), String> {
        use std::sync::Arc;
        let chidori = self.chidori.clone();
        let mut file_watch_guard = self.file_watch.lock().expect("Failed to lock file_watch");

        // Initialize the watcher and set up the event handler within a single block to avoid cloning `path` multiple times.
        let watcher_chidori = chidori.clone();
        let watcher_path = path.clone();
        let local_cell_state = self.local_cell_state.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(200),
            None,
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => events.iter().for_each(|event| {}),
                    Err(errors) => errors.iter().for_each(|error| {}),
                }
                let path_buf = PathBuf::from(&watcher_path);
                let mut chidori_guard = watcher_chidori.lock().expect("Failed to lock chidori");
                let cell_holders = chidori_guard.load_md_directory(&path_buf).expect("Failed to load markdown directory");
                for cell in cell_holders {
                    local_cell_state.insert(cell.op_id, Arc::new(std::sync::Mutex::new(CellState {
                        cell: Some(cell),
                        ..Default::default()
                    })));
                }
            },
        )
        .unwrap();

        // Watch the directory for changes. Since `path` has not been moved, we can reuse it here.
        debouncer
            .watcher()
            .watch(Path::new(&path), RecursiveMode::Recursive)
            .expect("Failed to watch directory");
        debouncer
            .cache()
            .add_root(Path::new(&path), RecursiveMode::Recursive);

        // Replace the old watcher with the new one.
        *file_watch_guard = Some(debouncer);

        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            let cell_holders = chidori_guard
                .load_md_directory(Path::new(&path))
                .map_err(|e| e.to_string())?;
            for cell in cell_holders {
                self.local_cell_state.insert(cell.op_id, Arc::new(std::sync::Mutex::new(CellState {
                    cell: Some(cell),
                    ..Default::default()
                })));
            }
        }
        Ok(())
    }
} 