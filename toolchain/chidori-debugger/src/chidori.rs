use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::bevy_egui::EguiContexts;
use bevy::app::{App, AppExit, Startup, Update};
use bevy::input::ButtonInput;
use bevy::prelude::{default, Commands, KeyCode, Local, Res, ResMut, Resource, EventReader, NextState, EventWriter};
use bevy_utils::tracing::{debug, error, info};
use dashmap::mapref::one::Ref;
use chidori_core::uuid::Uuid;
use egui;
use egui::panel::TopBottomSide;
use egui::{Color32, FontFamily, Frame, Id, Margin, Response, Rgba, Vec2b, Visuals, Widget};
use egui_tiles::{TabState, Tile, TileId, Tiles};
use notify_debouncer_full::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode, Watcher},
    DebounceEventResult, Debouncer, FileIdMap,
};

use crate::{tokio_tasks, CurrentTheme, MenuAction, GameState};
use chidori_core::execution::execution::execution_graph::{
    ExecutionNodeId, MergedStateHistory,
};
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::sdk::interactive_chidori_wrapper::{InteractiveChidoriWrapper, EventsFromRuntime};
use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
use chidori_core::tokio::task::JoinHandle;
use chidori_core::utils::telemetry::TraceEvents;
use petgraph::graph::NodeIndex;
use petgraph::prelude::StableGraph;
use chidori_core::cells::TextRange;
use chidori_core::sdk::chidori_runtime_instance::{PlaybackState, UserInteractionMessage};
use chidori_core::sdk::md::cell_type_to_markdown;

const RECV_RUNTIME_EVENT_TIMEOUT_MS: u64 = 100;

#[derive(Debug)]
pub struct Pane {
    pub tile_id: Option<TileId>,
    pub nr: String,
    pub rect: Option<egui::Rect>,
}

struct TreeBehavior<'a> {
    current_theme: &'a CurrentTheme
}

impl<'a> egui_tiles::Behavior<Pane> for TreeBehavior<'a> {
    fn tab_bar_color(&self, visuals: &Visuals) -> Color32 {
        if visuals.dark_mode {
            (Rgba::from(visuals.panel_fill) * Rgba::from_gray(0.8)).into()
        } else {
            (Rgba::from(visuals.panel_fill) * Rgba::from_gray(0.8)).into()
        }
    }

    /// The background color of a tab.
    fn tab_bg_color(
        &self,
        visuals: &Visuals,
        _tiles: &Tiles<Pane>,
        _tile_id: TileId,
        state: &TabState,
    ) -> Color32 {
        if state.active {
            visuals.panel_fill // same as the tab contents
        } else {
            Color32::TRANSPARENT // fade into background
        }
    }

    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tile_id: egui_tiles::TileId,
        pane: &mut Pane,
    ) -> egui_tiles::UiResponse {
        pane.tile_id = Some(tile_id.clone());
        pane.rect = Some(ui.max_rect());
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::widget_text::WidgetText {
        format!("{}", pane.nr).into()
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions {
            join_nested_linear_containers: true,
            prune_single_child_tabs: true,
            prune_empty_containers: true,
            prune_single_child_containers: true,
            prune_empty_tabs: true,
            all_panes_must_have_tabs: true,
            ..default()
        }
    }
}

#[derive(Resource, Default)]
pub struct EguiTreeIdentities {
    pub code_tile: Option<TileId>,
    pub logs_tile: Option<TileId>,
    pub graph_tile: Option<TileId>,
    pub traces_tile: Option<TileId>,
    pub chat_tile: Option<TileId>,
}

#[derive(Resource)]
pub struct EguiTree {
    pub tree: egui_tiles::Tree<Pane>,
}

fn keyboard_shortcut_tab_focus(
    mut identities: ResMut<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    button_input: Res<ButtonInput<KeyCode>>,
) {
    if button_input.pressed(KeyCode::SuperLeft) {
        if button_input.just_pressed(KeyCode::KeyT) {
            tree.tree.make_active(|id, _| {
                id == identities.traces_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyL) {
            tree.tree.make_active(|id, _| {
                id == identities.logs_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyG) {
            tree.tree.make_active(|id, _| {
                id == identities.graph_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyC) {
            tree.tree.make_active(|id, _| {
                id == identities.code_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyH) {
            tree.tree.make_active(|id, _| {
                id == identities.chat_tile.unwrap()
            });
        }
    }

}


fn maintain_egui_tree_identities(
    mut identities: ResMut<EguiTreeIdentities>,
    tree: ResMut<EguiTree>
) {
    tree.tree.tiles.iter().for_each(|(tile_id, tile)| {
        match tile {
            Tile::Pane(p) => {
                if &p.nr == &"Code" {
                    identities.code_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Logs" {
                    identities.logs_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Graph" {
                    identities.graph_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Traces" {
                    identities.traces_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Chat" {
                    identities.chat_tile = Some(tile_id.clone());
                }
            }
            _ => {}
        }
    })

}

impl Default for EguiTree {
    fn default() -> Self {
        let mut next_view_nr = 0;
        let mut gen_pane = |name: String| {
            let pane = Pane {
                tile_id: None,
                nr: name,
                rect: None,
            };
            next_view_nr += 1;
            pane
        };

        let mut tiles = egui_tiles::Tiles::default();

        let tabs = vec![
            tiles.insert_pane(gen_pane(String::from("Code"))),
            tiles.insert_pane(gen_pane(String::from("Logs"))),
            tiles.insert_pane(gen_pane(String::from("Graph"))),
            tiles.insert_pane(gen_pane(String::from("Traces"))),
            tiles.insert_pane(gen_pane(String::from("Chat")))
        ];
        let root = tiles.insert_tab_tile(tabs);

        EguiTree {
            tree: egui_tiles::Tree::new("my_tree", root, tiles),
        }
    }
}

const EXAMPLES_CORE1: &str = include_str!("../examples/core1_simple_math/core.md");
const EXAMPLES_CORE2: &str = include_str!("../examples/core2_marshalling/core.md");
const EXAMPLES_CORE3: &str =
    include_str!("../examples/core3_function_invocations/core.md");
const EXAMPLES_CORE4: &str =
    include_str!("../examples/core4_async_function_invocations/core.md");
const EXAMPLES_CORE5: &str =
    include_str!("../examples/core5_prompts_invoked_as_functions/core.md");
const EXAMPLES_CORE6: &str =
    include_str!("../examples/core6_prompts_leveraging_function_calling/core.md");
const EXAMPLES_CORE7: &str =
    include_str!("../examples/core7_rag_stateful_memory_cells/core.md");
const EXAMPLES_CORE8: &str =
    include_str!("../examples/core8_prompt_code_generation_and_execution/core.md");
const EXAMPLES_CORE9: &str =
    include_str!("../examples/core9_multi_agent_simulation/core.md");
const EXAMPLES_CORE10: &str =
    include_str!("../examples/core10_concurrency/core.md");
const EXAMPLES_CORE11: &str =
    include_str!("../examples/core11_hono/core.md");
const EXAMPLES_CORE12: &str =
    include_str!("../examples/core12_dependency_management/core.md");

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
}


#[derive(Default)]
pub struct CellState {
    pub(crate) is_repl_open: bool,
    pub(crate) is_new_cell_open: bool,
    pub(crate) repl_content: String,
    pub(crate) json_content: serde_json::Value,
    pub(crate) cell: Option<CellHolder>
}


#[derive(Resource)]
pub struct ChidoriState {
    /// Toggles visualization of debug information
    pub debug_mode: bool,

    /// What is the current path on the file system that we're observing
    pub(crate) watched_path: Mutex<Option<String>>,

    /// Handler for watching the watched_path directory
    file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,

    /// Retain thread handle for the Chidori runtime
    background_thread: Mutex<Option<JoinHandle<()>>>,

    /// Instance of the InteractiveChidoriWrapper
    pub chidori: Arc<Mutex<InteractiveChidoriWrapper>>,

    /// Toggles display of the initialization modal
    pub application_state_is_displaying_example_modal: bool,
    pub application_state_is_displaying_save_dialog: bool,

    pub current_playback_state: PlaybackState,

    pub execution_id_to_evaluation: Arc<dashmap::DashMap<OperationId, ExecutionState>>,
    pub local_cell_state: Arc<dashmap::DashMap<OperationId, Arc<Mutex<CellState>>>>,

    pub log_messages: Vec<String>,

    pub definition_graph: Vec<(OperationId, OperationId, Vec<DependencyReference>)>,


    pub execution_graph: Vec<(ExecutionNodeId, ExecutionNodeId)>,
    pub grouped_nodes: HashSet<ExecutionNodeId>,
    pub current_execution_head: ExecutionNodeId,

    pub trace_events: Vec<TraceEvents>,
}


impl Default for ChidoriState {
    fn default() -> Self {
        ChidoriState {
            debug_mode: false,
            chidori: Arc::new(Mutex::new(InteractiveChidoriWrapper::new())),
            watched_path: Mutex::new(None),
            background_thread: Mutex::new(None),
            file_watch: Mutex::new(None),
            application_state_is_displaying_example_modal: true,
            application_state_is_displaying_save_dialog: false,
            current_playback_state: PlaybackState::Paused,
            execution_id_to_evaluation: Arc::new(Default::default()),
            local_cell_state: Default::default(),
            log_messages: vec![],
            definition_graph: vec![],
            execution_graph: vec![],
            grouped_nodes: Default::default(),
            current_execution_head: Default::default(),
            trace_events: vec![],
        }
    }
}

impl ChidoriState {

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
        self.watched_path = Mutex::new(None);
        self.background_thread = Mutex::new(None);
        self.file_watch = Mutex::new(None);
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
        self.application_state_is_displaying_example_modal = false;
        let chidori = self.chidori.clone();
        let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
        let cell_holders = chidori_guard.load_md_string(file_content).expect("Failed to load markdown string");
        for cell in cell_holders {
            self.local_cell_state.insert(cell.op_id, Arc::new(Mutex::new(crate::chidori::CellState {
                cell: Some(cell),
                ..default()
            })));
        }
        Ok(())
    }

    pub fn load_and_watch_directory(&self, path: String) -> anyhow::Result<(), String> {
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
                    local_cell_state.insert(cell.op_id, Arc::new(Mutex::new(crate::chidori::CellState {
                        cell: Some(cell),
                        ..default()
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
                self.local_cell_state.insert(cell.op_id, Arc::new(Mutex::new(crate::chidori::CellState {
                    cell: Some(cell),
                    ..default()
                })));
            }
        }
        Ok(())
    }
}

fn setup(mut commands: Commands, runtime: ResMut<tokio_tasks::TokioTasksRuntime>) {
    let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
    let mut internal_state = ChidoriState {
        debug_mode: false,
        chidori: Arc::new(Mutex::new(InteractiveChidoriWrapper::new_with_events(
            trace_event_sender,
            runtime_event_sender,
        ))),
        watched_path: Mutex::new(None),
        background_thread: Mutex::new(None),
        file_watch: Mutex::new(None),
        application_state_is_displaying_example_modal: true,
        application_state_is_displaying_save_dialog: false,
        current_playback_state: PlaybackState::Paused,
        execution_id_to_evaluation: Arc::new(Default::default()),
        local_cell_state: Default::default(),
        log_messages: vec![],
        definition_graph: vec![],
        execution_graph: vec![],
        grouped_nodes: Default::default(),
        current_execution_head: Default::default(),
        trace_events: vec![],
    };

    // Clone a reference to the shared execution_id_to_evaluation
    {
        let mut chidori = internal_state
            .chidori
            .lock()
            .expect("Failed to lock background_thread");
        let shared_state = chidori.shared_state.lock().unwrap();
        internal_state.execution_id_to_evaluation = shared_state.execution_id_to_evaluation.clone();
    }

    {
        let mut background_thread_guard = internal_state
            .background_thread
            .lock()
            .expect("Failed to lock background_thread");
        let chidori = internal_state.chidori.clone();
        *background_thread_guard = Some(runtime.spawn_background_task(|mut ctx| async move {
            loop {
                // Create an instance within the loop
                let mut instance = {
                    let mut chidori_guard = chidori.lock().unwrap();
                    let instance = chidori_guard.get_instance().unwrap();
                    drop(chidori_guard); // Drop the lock on chidori to avoid deadlock
                    instance
                };

                let _ = instance.wait_until_ready().await;
                let result = instance.run(PlaybackState::Paused).await;
                match result {
                    Ok(_) => {
                        panic!("Instance completed execution and closed successfully.");
                    }
                    Err(e) => {
                        println!("Error occurred: {}, retrying...", e);
                    }
                }
            }
        }));
    }

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match runtime_event_receiver.recv_timeout(Duration::from_millis(RECV_RUNTIME_EVENT_TIMEOUT_MS)) {
                Ok(msg) => {
                    debug!("Received message from runtime: {:?}", &msg);
                    let msg_to_logs = msg.clone();
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) =
                            ctx.world.get_resource_mut::<ChidoriState>()
                        {
                            s.log_messages.push(format!("Received from runtime: {:?}", &msg_to_logs));
                        }
                    })
                        .await;
                    match msg {
                        EventsFromRuntime::ExecutionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriState>()
                                {
                                    s.execution_graph = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::UpdateExecutionHead(head) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriState>()
                                {
                                    s.current_execution_head = head;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ReceivedChatMessage(_) => {}
                        EventsFromRuntime::PlaybackState(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut internal_state) = ctx.world.get_resource_mut::<ChidoriState>() {
                                    internal_state.current_playback_state = state;
                                }
                            })
                                .await;
                        }
                    }
                }
                Err(e) => match e {
                    RecvTimeoutError::Timeout => {}
                    RecvTimeoutError::Disconnected => {
                        println!("Runtime channel disconnected");
                        break;
                    }
                },
            }
        }
        println!("Runtime event loop ended");
    });

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match trace_event_receiver.recv() {
                Ok(msg) => {
                    // println!("Received: {:?}", &msg);
                    // handle.emit_all("execution:events", Some(trace_event_to_string(msg)));
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriState>() {
                            s.trace_events.push(msg);
                        }
                    })
                    .await;
                }
                Err(_) => {
                    println!("Channel closed");
                    break;
                }
            }
        }
    });

    commands.insert_resource(internal_state);
}

fn with_cursor(res: Response) -> Response {
    if res.hovered() {
        res.ctx.output_mut(|p| {
            p.cursor_icon = egui::CursorIcon::PointingHand;
        });
    }
    res
}


pub fn initial_save_notebook_dialog(
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut notebook_name_state: Local<Option<String>>,
) {
    if !internal_state.application_state_is_displaying_save_dialog {
        return;
    }
    let mut contexts1 = &mut contexts;
    let mut internal_state1 = &mut internal_state;

    // Initialize the Local state if it's None
    if notebook_name_state.is_none() {
        *notebook_name_state = Some(String::new());
    }

    let mut saving_notebook_name = Some(String::new());
    egui::CentralPanel::default()
        .frame(
            Frame::default()
                .fill(theme.theme.card)
                .stroke(theme.theme.card_border)
                .inner_margin(16.0)
                .outer_margin(200.0)
                .rounding(theme.theme.radius as f32),
        )
        .show(contexts1.ctx_mut(), |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Save Notebook");
                ui.add_space(8.0);

                // Get a mutable reference to the String inside the Option
                {
                    let mut notebook_name = notebook_name_state.as_mut().unwrap();
                    let response = ui.add(
                        egui::TextEdit::singleline(notebook_name)
                            .hint_text("Enter notebook name...")
                            .desired_width(300.0)
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if !notebook_name.trim().is_empty() {
                            internal_state1.save_notebook();
                        }
                    }
                }

                ui.add_space(16.0);

                // Buttons row
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        saving_notebook_name = Some(String::new());
                        internal_state1.application_state_is_displaying_save_dialog = false;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let notebook_name = notebook_name_state.as_mut().unwrap();
                        let save_button = ui.add_enabled(
                            !notebook_name.trim().is_empty(),
                            egui::Button::new("Save")
                        );

                        if save_button.clicked() {
                            // internal_state1.save_notebook(notebook_name, &runtime);
                            saving_notebook_name = Some(String::new());
                            internal_state1.application_state_is_displaying_save_dialog = false;
                        }
                    });
                });
            });
        });
}



fn handle_menu_actions(
    mut menu_events: EventReader<MenuAction>,
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut displayed_example_desc: Local<Option<(String, String, String)>>
) {
    for event in menu_events.read() {
        match event {
            MenuAction::NewProject => {}
            MenuAction::OpenProject => {
                internal_state.application_state_is_displaying_example_modal = false;
                // let sender = self.text_channel.0.clone();
                runtime.spawn_background_task(|mut ctx| async move {
                    let task = rfd::AsyncFileDialog::new().pick_folder();
                    let folder = task.await;
                    if let Some(folder) = folder {
                        let path = folder.path().to_string_lossy().to_string();
                        ctx.run_on_main_thread(move |ctx| {
                            if let Some(mut internal_state) =
                                ctx.world.get_resource_mut::<ChidoriState>()
                            {
                                match internal_state.load_and_watch_directory(path) {
                                    Ok(()) => {
                                        // Directory loaded and watched successfully
                                        println!("Directory loaded and being watched successfully");
                                    },
                                    Err(e) => {
                                        // Handle the error
                                        eprintln!("Error loading and watching directory: {}", e);
                                    }
                                }
                            }
                        })
                            .await;
                    }
                });
            }
            MenuAction::Save => {
                internal_state.save_notebook();
            }
            _ => {}
        }
    }
}

pub fn root_gui(
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut displayed_example_desc: Local<Option<(String, String, String)>>
) {
    if internal_state.application_state_is_displaying_example_modal {
        let mut contexts1 = &mut contexts;
        let mut internal_state1 = &mut internal_state;
        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(theme.theme.card)
                    .stroke(theme.theme.card_border)
                    .inner_margin(16.0)
                    .outer_margin(100.0)
                    .rounding(theme.theme.radius as f32),
            )
            .show(contexts1.ctx_mut(), |ui| {
                ui.add_space(12.0);
                let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
                {
                    let mut ui = &mut frame.content_ui;
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 12.0);
                    // Add widgets inside the frame
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label("New Notebook:");
                            let res = with_cursor(ui.button("Create New Notebook"));
                            if res.clicked() {
                                internal_state1.application_state_is_displaying_example_modal = false;
                                internal_state1.application_state_is_displaying_save_dialog = true;
                            }
                            ui.add_space(16.0);
                            ui.label("Load Existing Project");
                            let res = with_cursor(ui.button("Load From Folder"));
                            if res.clicked() {
                                internal_state1.application_state_is_displaying_example_modal = false;
                                runtime.spawn_background_task(|mut ctx| async move {
                                    let task = rfd::AsyncFileDialog::new().pick_folder();
                                    let folder = task.await;
                                    if let Some(folder) = folder {
                                        let path = folder.path().to_string_lossy().to_string();
                                        ctx.run_on_main_thread(move |ctx| {
                                            if let Some(mut internal_state) =
                                                ctx.world.get_resource_mut::<ChidoriState>()
                                            {
                                                let mut watched_path = internal_state.watched_path.get_mut().unwrap();
                                                *watched_path = Some(path.clone());
                                                match internal_state.load_and_watch_directory(path) {
                                                    Ok(()) => {
                                                        // Directory loaded and watched successfully
                                                        info!("Directory loaded and being watched successfully");
                                                    },
                                                    Err(e) => {
                                                        // Handle the error
                                                        error!("Error loading and watching directory: {}", e);
                                                    }
                                                }
                                            }
                                        })
                                            .await;
                                    }
                                });
                            }
                            ui.add_space(16.0);
                            ui.label("Load Example:");
                            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
                            let buttons_text_load = vec![
                                ("Core 1: Simple Math", EXAMPLES_CORE1, "Demonstrates simple arithmetic between cells, and that values can be passed between Python and JavaScript runtimes."),
                                ("Core 2: Marshalling Values", EXAMPLES_CORE2, "All of the types that we can successfully pass between runtimes and that are preserved by our execution engine."),
                                ("Core 3: Invoking Functions", EXAMPLES_CORE3, "Demonstrates what function execution looks like when using Chidori. Explore how states are preserved and the ability to revert between them with re-execution."),
                                ("Core 4: Invoking Async Functions", EXAMPLES_CORE4, "Function invocations default to being asynchronous."),
                                ("Core 5: Invoking Prompts as Functions", EXAMPLES_CORE5, "We treat prompts as first class resources, this demonstrates how prompts are invokable as functions."),
                                (
                                    "Core 6: Using Function Calling in Prompts",
                                    EXAMPLES_CORE6, "Prompts may import functions and invoke those in order to accomplish their instructions."

                                ),
                                ("Core 7: Chat With PDF Clone", EXAMPLES_CORE7, "Cells preserve their internal state, we provide a specialized API for embeddings which demonstrates this behavior, exposing functions for interacting with that state."),
                                (
                                    "Core 8: Anthropic Artifacts Clone",
                                    EXAMPLES_CORE8, "Chidori is designed for L4-L5 agents, new behaviors can be generated on the fly via code generation."
                                ),
                                ("Core 9: Multi-Agent Social Experiment", EXAMPLES_CORE9, "desc"),
                                ("Core 10: Demonstrating Our Execution Concurrency", EXAMPLES_CORE10, "desc"),
                                ("Core 11: Hono Web Service", EXAMPLES_CORE11, "desc"),
                                ("Core 12: Dependency Management", EXAMPLES_CORE12, "desc"),
                            ];


                            let available_height = ui.available_height();
                            egui::ScrollArea::vertical()
                                .auto_shrink(Vec2b::new(true, false))
                                .min_scrolled_height(400.0).show(ui, |ui| {
                                let mut frame = egui::Frame::default().outer_margin(Margin {
                                    left: 0.0,
                                    right: 40.0,
                                    top: 0.0,
                                    bottom: 0.0,
                                }).rounding(6.0).begin(ui);
                                {
                                    ui.set_height(available_height);
                                    let mut ui = &mut frame.content_ui;
                                    let mut is_a_button_hovered = false;
                                    for button in buttons_text_load {
                                        let res = with_cursor(ui.button(button.0));
                                        if res.hovered() {
                                            is_a_button_hovered = true;
                                            *displayed_example_desc = Some((button.0.to_string(), button.1.to_string(), button.2.to_string()));
                                        }
                                        if res.clicked() {
                                            internal_state1.load_string(button.1);
                                        }
                                    }
                                    if is_a_button_hovered == false {
                                        *displayed_example_desc = None;
                                    }
                                }
                                frame.end(ui);
                            });
                        });

                        if let Some((title, code, desc)) = &*displayed_example_desc {
                            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                                let mut frame = egui::Frame::default().outer_margin(Margin::symmetric(64.0, 0.0)).inner_margin(64.0).rounding(6.0).begin(ui);
                                {
                                    let mut ui = &mut frame.content_ui;
                                    let mut code_mut = code.to_string();
                                    ui.set_max_width(800.0);
                                    ui.label(title);
                                    ui.add_space(16.0);
                                    ui.label(desc);
                                    ui.add_space(16.0);
                                    egui::ScrollArea::new([false, true]) // Horizontal: false, Vertical: true
                                        .max_width(800.0)
                                        .max_height(600.0)
                                        .show(ui, |ui| {
                                            ui.add(
                                                egui::TextEdit::multiline(&mut code_mut)
                                                    .font(egui::FontId::new(14.0, FontFamily::Monospace))
                                                    .code_editor()
                                                    .lock_focus(true)
                                                    .desired_width(f32::INFINITY)
                                                    .margin(Margin::symmetric(8.0, 8.0))

                                            );
                                        });
                                }
                                frame.end(ui);
                            });
                        }

                        // ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        //     ui.add_space(ui.available_height() / 2.0 - 256.0); // Center vertically
                        //     egui::Image::new(egui::include_image!("../assets/images/tblogo-white.png"))
                        //         .fit_to_exact_size(vec2(512.0, 512.0))
                        //         .rounding(5.0)
                        //         .ui(ui);
                        // });
                    });
                }
                frame.end(ui);
            });
    } else {
        egui::CentralPanel::default()
            .frame(egui::Frame::default().outer_margin(Margin {
                left: 0.0,
                right: 0.0,
                top: 48.0,
                bottom: 0.0,
            }))
            .show(contexts.ctx_mut(), |ui| {
                let mut behavior = TreeBehavior {
                    current_theme: &theme
                };
                egui_tree.tree.ui(&mut behavior, ui);
            });

    }

    egui::TopBottomPanel::new(TopBottomSide::Top, Id::new("top_panel")).show(
        contexts.ctx_mut(),
        |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
            // ui.text_edit_multiline(&mut text);
            // a simple button opening the dialog
            let mut frame = egui::Frame::default()
                .inner_margin(Margin::symmetric(8.0, 8.0))
                .begin(ui);
            {
                let mut ui = &mut frame.content_ui;
                ui.horizontal(|ui| {
                    ui.style_mut().spacing.item_spacing = egui::vec2(32.0, 8.0);
                    // if with_cursor(ui.button("Save")).clicked() {
                    //     internal_state.reset();
                    // }
                    // if with_cursor(ui.button("Reset")).clicked() {
                    //     internal_state.reset();
                    // }
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);

                    if !internal_state.application_state_is_displaying_example_modal {
                        match internal_state.current_playback_state {
                            PlaybackState::Paused => {
                                if with_cursor(ui.button("⏵")).clicked() {
                                    internal_state.play();
                                }
                                if with_cursor(ui.button("⏭")).clicked() {
                                    internal_state.step();
                                }
                            }
                            PlaybackState::Step => {
                                if with_cursor(ui.button("⏵️")).clicked() {
                                    internal_state.play();
                                }
                                if with_cursor(ui.button("⏸")).clicked() {
                                    internal_state.pause();
                                }
                            }
                            PlaybackState::Running => {
                                if with_cursor(ui.button("⏸")).clicked() {
                                    internal_state.pause();
                                }
                            }
                        }
                    }

                    ui.add_space(8.0);

                    // let mut my_f32 = 0.0;
                    // ui.add(egui::Slider::new(&mut my_f32, 0.0..=100.0).text("Rate Limit func/s"));

                    // ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    //     if with_cursor(ui.button("UI Debug Mode")).clicked() {
                    //         internal_state.debug_mode = !internal_state.debug_mode;
                    //     }
                    // });
                });

            }
            frame.end(ui);
        },
    );
}

pub fn chidori_plugin(app: &mut App) {
    app.init_resource::<EguiTree>()
        .init_resource::<EguiTreeIdentities>()
        .add_systems(Update, (
            handle_menu_actions,
            root_gui,
            initial_save_notebook_dialog,
            maintain_egui_tree_identities
        ))
        .add_systems(Startup, setup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use chidori_core::cells::{CellTypes, CodeCell, SupportedLanguage};

    fn create_test_cell(op_id: OperationId, path: &str, range: TextRange, body: &str, is_dirty: bool) -> CellHolder {
        let cell = CodeCell {
            backing_file_reference: Some(chidori_core::cells::BackingFileReference {
                path: path.to_string(),
                text_range: Some(range),
            }),
            // Add any other required fields with their default values
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: body.to_string(),
            function_invocation: None,
        };
        
        CellHolder {
            op_id,
            cell: CellTypes::Code(cell, TextRange::default()),
            is_dirty_editor: is_dirty,
        }
    }

    #[test]
    fn test_save_notebook_single_file() {
        // Create a temporary directory
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        // Create initial file content
        let initial_content = "def hello():\n    print('hello')\n\ndef world():\n    print('world')\n";
        fs::write(&file_path, initial_content).unwrap();

        // Create ChidoriState with a modified cell
        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();
        
        // Calculate the correct range for the first function
        let first_func_range = TextRange { 
            start: 0, 
            end: initial_content.find("\n\ndef").unwrap_or(initial_content.len())
        };
        
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            first_func_range,
            "def hello():\n    print('modified')",
            true
        );

        let cell_state = CellState {
            cell: Some(cell_holder),
            ..Default::default()
        };

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(cell_state)));

        // Save the notebook
        state.save_notebook();

        // Verify the file content
        let final_content = fs::read_to_string(&file_path).unwrap();
        let expected_content = "def hello():\n    print('modified')\n\ndef world():\n    print('world')\n";
        assert_eq!(final_content, expected_content);
    }

    #[test]
    fn test_save_notebook_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let file1_path = temp_dir.path().join("file1.py");
        let file2_path = temp_dir.path().join("file2.py");

        // Create initial file contents with explicit newlines
        let file1_content = "def func1():\n    return 1\n";
        let file2_content = "def func2():\n    return 2\n";
        fs::write(&file1_path, file1_content).unwrap();
        fs::write(&file2_path, file2_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id1 = Uuid::now_v7();
        let op_id2 = Uuid::now_v7();

        // Create modified cells with correct ranges and content
        let cell_holder1 = create_test_cell(
            op_id1,
            file1_path.to_str().unwrap(),
            TextRange { start: 0, end: file1_content.len() },
            "def func1():\n    return 'modified1'\n",
            true
        );

        let cell_holder2 = create_test_cell(
            op_id2,
            file2_path.to_str().unwrap(),
            TextRange { start: 0, end: file2_content.len() },
            "def func2():\n    return 'modified2'\n",
            true
        );

        state.local_cell_state.insert(op_id1, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder1),
            ..Default::default()
        })));

        state.local_cell_state.insert(op_id2, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder2),
            ..Default::default()
        })));

        // Save the notebook
        state.save_notebook();

        // Verify both files' content
        let content1 = fs::read_to_string(&file1_path).unwrap();
        let content2 = fs::read_to_string(&file2_path).unwrap();

        assert_eq!(content1, "def func1():\n    return 'modified1'\n");
        assert_eq!(content2, "def func2():\n    return 'modified2'\n");
    }

    #[test]
    fn test_save_notebook_non_dirty_cells() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        let initial_content = "def test():\n    return 1\n";
        fs::write(&file_path, initial_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();

        // Create a cell that is not dirty
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            TextRange { start: 0, end: 21 },
            "def test():\n    return 'modified'\n",
            false // Not dirty
        );

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder),
            ..Default::default()
        })));

        // Save the notebook
        state.save_notebook();

        // Verify the file content remains unchanged
        let final_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(final_content, initial_content);
    }

    #[test]
    fn test_save_notebook_invalid_range() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        let initial_content = "def test():\n    return 1\n";
        fs::write(&file_path, initial_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();

        // Create a cell with an invalid range
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            TextRange { start: 1000, end: 2000 }, // Invalid range
            "def test():\n    return 'modified'\n",
            true
        );

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder),
            ..Default::default()
        })));

        // Save should not panic and file should remain unchanged
        state.save_notebook();

        let final_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(final_content, initial_content);
    }
}

