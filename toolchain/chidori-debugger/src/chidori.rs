use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use bevy::app::{App, Startup, Update};
use bevy::input::ButtonInput;
use bevy::prelude::{Commands, default, KeyCode, Local, NextState, Res, ResMut, Resource};
use crate::bevy_egui::EguiContexts;
use egui;
use egui::{Color32, FontFamily, Frame, Id, Margin, Response, Vec2b, Visuals, Widget};
use egui::panel::TopBottomSide;
use egui_tiles::{Tile, TileId};
use notify_debouncer_full::{
    DebounceEventResult,
    Debouncer,
    FileIdMap, new_debouncer, notify::{RecommendedWatcher, RecursiveMode, Watcher},
};
use chidori_core::uuid::Uuid;

use chidori_core::execution::execution::execution_graph::{
    ExecutionNodeId, MergedStateHistory,
};
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::sdk::entry::{CellHolder, EventsFromRuntime, PlaybackState, UserInteractionMessage};
use chidori_core::tokio::task::JoinHandle;
use chidori_core::utils::telemetry::TraceEvents;
use petgraph::prelude::StableGraph;
use petgraph::graph::NodeIndex;
use chidori_core::execution::execution::execution_state::ExecutionStateEvaluation;
use chidori_core::sdk::chidori::Chidori;
use crate::{CurrentTheme, GameState, tokio_tasks};

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

    // fn tab_bar_color(&self, visuals: &Visuals) -> Color32 {
    //     self.current_theme.theme.card
    // }

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

#[derive(Resource)]
pub struct ChidoriTraceEvents {
    pub inner: Vec<TraceEvents>,
}

#[derive(Resource)]
pub struct ChidoriExecutionIdsToStates {
    pub inner: HashMap<ExecutionNodeId, ExecutionState>,
}

#[derive(Resource)]
pub struct ChidoriExecutionGraph {
    pub execution_graph: Vec<(ExecutionNodeId, ExecutionNodeId)>,
    pub grouped_nodes: HashSet<ExecutionNodeId>,
    pub current_execution_head: ExecutionNodeId,
}

impl Default for ChidoriExecutionGraph {
    fn default() -> Self {
        ChidoriExecutionGraph {
            execution_graph: vec![],
            grouped_nodes: Default::default(),
            current_execution_head: Uuid::nil(),
        }
    }
}

fn hash_graph(input: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

impl ChidoriExecutionGraph {
    pub fn construct_stablegraph_from_chidori_execution_graph(&self) -> (StableGraph<ExecutionNodeId, ()>, HashMap<ExecutionNodeId, NodeIndex>) {
        let execution_graph = &self.execution_graph;
        let mut dataset = StableGraph::new();
        let mut node_ids = HashMap::new();
        // for (a, b) in &execution_graph.stack_graph
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


    pub fn exists_in_current_tree(&self, n: &ExecutionNodeId) -> bool {
        let h = self.current_execution_head;
        let (graph, nodes) = self.construct_stablegraph_from_chidori_execution_graph();
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

#[derive(Resource)]
pub struct ChidoriDefinitionGraph {
    pub inner: Vec<(OperationId, OperationId, Vec<DependencyReference>)>,
}

#[derive(Resource)]
pub struct ChidoriExecutionState {
    pub inner: Option<MergedStateHistory>,
}



#[derive(Resource, Default)]
pub struct ChidoriLogMessages {
    pub inner: Vec<String>,
}

#[derive(Resource)]
pub struct ChidoriCells {
    pub editor_cells: Vec<CellHolder>,
    pub state_cells: Vec<CellHolder>,
}

#[derive(Resource)]
pub struct ChidoriState {
    pub debug_mode: bool,
    watched_path: Mutex<Option<String>>,
    file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,
    background_thread: Mutex<Option<JoinHandle<()>>>,
    pub chidori: Arc<Mutex<Chidori>>,
    pub display_example_modal: bool,
    pub current_playback_state: PlaybackState



}

impl ChidoriState {
    pub fn get_loaded_path(&self) -> String {
        let env = self.chidori.lock().unwrap();
        if env.loaded_path.is_none() {
            return "".to_string();
        }
        env.loaded_path.as_ref().unwrap().to_string()
    }

    // pub fn move_state_view_to_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
    //     let env = self.chidori.lock().unwrap();
    //     env.handle_user_action(UserInteractionMessage::RevertToState(Some(id)))
    //         .map_err(|e| e.to_string())?;
    //     Ok(())
    // }

    pub fn get_execution_state_at_id(
        &self,
        execution_node_id: &ExecutionNodeId,
    ) -> Option<ExecutionStateEvaluation> {
        // TODO: this is like 3 locks just to get the current state - maybe we should cache these?
        let chidori = self.chidori.lock().unwrap();
        let eval = {
            let shared_state = chidori.get_shared_state();
            let exec = shared_state.execution_id_to_evaluation.clone();
            let eval = exec.get(&execution_node_id).map(|x| x.clone());
            eval
        };
        eval
    }


    pub fn step(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Step)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn play(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Play)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pause(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Pause)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_execution_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
        // TODO: we're failing to lock chidori
        println!("=== lock chidori");
        let chidori = self.chidori.clone();
        {
            let chidori_guard = chidori.lock().expect("Failed to lock chidori");
            println!("=== handle user action Revert");
            chidori_guard.handle_user_action(UserInteractionMessage::RevertToState(Some(id)))
                .map_err(|e| e.to_string())?;

        }
        println!("=== set execution id, drop the lock");
        Ok(())
    }

    pub fn update_cell(&self, cell_holder: CellHolder) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        {
            let chidori_guard = chidori.lock().expect("Failed to lock chidori");
            chidori_guard.handle_user_action(UserInteractionMessage::MutateCell(cell_holder))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn load_string(&mut self, file: &str) -> anyhow::Result<(), String> {
        self.display_example_modal = false;
        let chidori = self.chidori.clone();
        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            chidori_guard.load_md_string(file).expect("Failed to load markdown string");
        }
        Ok(())
    }

    pub fn load_and_watch_directory(&self, path: String) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        let mut file_watch_guard = self.file_watch.lock().expect("Failed to lock file_watch");

        // Initialize the watcher and set up the event handler within a single block to avoid cloning `path` multiple times.
        let watcher_chidori = chidori.clone();
        let watcher_path = path.clone();
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
                chidori_guard.load_md_directory(&path_buf).expect("Failed to load markdown directory");
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
            dbg!("Loading directory");
            chidori_guard
                .load_md_directory(Path::new(&path))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn setup(mut commands: Commands, runtime: ResMut<tokio_tasks::TokioTasksRuntime>) {
    commands.insert_resource(ChidoriTraceEvents { inner: vec![] });
    commands.insert_resource(ChidoriExecutionGraph {
        execution_graph: vec![],
        grouped_nodes: Default::default(),
        current_execution_head: Uuid::nil(),
    });
    commands.insert_resource(ChidoriExecutionIdsToStates {
        inner: HashMap::new(),
    });

    commands.insert_resource(ChidoriDefinitionGraph { inner: vec![] });
    commands.insert_resource(ChidoriExecutionState { inner: None });
    commands.insert_resource(ChidoriCells { editor_cells: vec![], state_cells: vec![] });

    let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
    let mut internal_state = ChidoriState {
        debug_mode: false,
        chidori: Arc::new(Mutex::new(Chidori::new_with_events(
            trace_event_sender,
            runtime_event_sender,
        ))),
        watched_path: Mutex::new(None),
        background_thread: Mutex::new(None),
        file_watch: Mutex::new(None),
        display_example_modal: true,
        current_playback_state: PlaybackState::Paused,
    };

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

                let await_ready = instance.wait_until_ready().await;
                let result = instance.run().await;
                match result {
                    Ok(_) => {
                        panic!("Instance completed execution and closed successfully.");
                        break;
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
                    // println!("Received from runtime: {:?}", &msg);
                    let msg_to_logs = msg.clone();
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) =
                            ctx.world.get_resource_mut::<ChidoriLogMessages>()
                        {
                            s.inner.push(format!("Received from runtime: {:?}", &msg_to_logs));
                        }
                    })
                        .await;
                    match msg {
                        EventsFromRuntime::ExecutionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionGraph>()
                                {
                                    s.execution_graph = state.0;
                                    s.grouped_nodes = state.1;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::StateAtId(id, state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionIdsToStates>()
                                {
                                    s.inner.insert(id, state);
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ExecutionStateChange(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionState>()
                                {
                                    s.inner = Some(state);
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::DefinitionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriDefinitionGraph>()
                                {
                                    s.inner = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::EditorCellsUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriCells>() {
                                    let mut sort_cells: Vec<CellHolder> = state.values().cloned().collect();
                                    sort_cells.sort_by(|a, b| {
                                        a.op_id.cmp(&b.op_id)
                                    });
                                    s.editor_cells = sort_cells;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::UpdateExecutionHead(head) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriExecutionGraph>()
                                {
                                    s.current_execution_head = head;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ReceivedChatMessage(_) => {}
                        EventsFromRuntime::ExecutionStateCellsViewUpdated(cells) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriCells>() {
                                    let mut sort_cells = cells.clone();
                                    sort_cells.sort_by(|a, b| {
                                        a.op_id.cmp(&b.op_id)
                                    });
                                    s.state_cells = sort_cells;
                                }
                            })
                                .await;

                        }
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
                        if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriTraceEvents>() {
                            s.inner.push(msg);
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

pub fn update_gui(
    mut commands: Commands,
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut state: ResMut<NextState<GameState>>,
    mut theme: Res<CurrentTheme>,
    mut displayed_example_desc: Local<Option<(String, String, String)>>
) {
    if internal_state.display_example_modal {
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
                                internal_state1.display_example_modal = false;
                            }
                            ui.add_space(16.0);
                            ui.label("Load Existing Project");
                            let res = with_cursor(ui.button("Load From Folder"));
                            if res.clicked() {
                                internal_state1.display_example_modal = false;
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


                            egui::ScrollArea::vertical().min_scrolled_height(400.0).show(ui, |ui| {
                                let mut frame = egui::Frame::default().outer_margin(Margin {
                                    left: 0.0,
                                    right: 40.0,
                                    top: 0.0,
                                    bottom: 0.0,
                                }).rounding(6.0).begin(ui);
                                {
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
                    if with_cursor(ui.button("Open")).clicked() {
                        internal_state.display_example_modal = false;
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
                    ui.add_space(8.0);
                    if with_cursor(ui.button("Examples")).clicked() {
                    }
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);

                    match internal_state.current_playback_state {
                        PlaybackState::Paused => {
                            if with_cursor(ui.button("Run")).clicked() {
                                internal_state.play();
                            }
                            if with_cursor(ui.button("Step")).clicked() {
                                internal_state.step();
                            }
                        }
                        PlaybackState::Step => {
                            if with_cursor(ui.button("Run")).clicked() {
                                internal_state.play();
                            }
                            if with_cursor(ui.button("Pause")).clicked() {
                                internal_state.pause();
                            }
                        }
                        PlaybackState::Running => {
                            if with_cursor(ui.button("Pause")).clicked() {
                                internal_state.pause();
                            }
                        }
                    }
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if with_cursor(ui.button("UI Debug Mode")).clicked() {
                            internal_state.debug_mode = !internal_state.debug_mode;
                        }
                    });
                });

            }
            frame.end(ui);
        },
    );
}

pub fn chidori_plugin(app: &mut App) {
    app.init_resource::<EguiTree>()
        .init_resource::<EguiTreeIdentities>()
        .init_resource::<ChidoriLogMessages>()
        .add_systems(Update, (update_gui, maintain_egui_tree_identities))
        .add_systems(Startup, setup);
}

