use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::RecvTimeoutError;

use bevy::prelude::{Resource};
use egui::{Color32, Id, Response, Visuals};
use egui_tiles::{TabState, Tile, TileId, Tiles};
use notify_debouncer_full::{
    Debouncer,
    FileIdMap, 
    notify::RecommendedWatcher,
};

use chidori_core::execution::execution::execution_graph::{
    ExecutionNodeId, MergedStateHistory,
};
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::sdk::interactive_chidori_wrapper::{InteractiveChidoriWrapper, CellHolder};
use chidori_core::tokio::task::JoinHandle;
use chidori_core::utils::telemetry::TraceEvents;
use chidori_core::sdk::chidori_runtime_instance::PlaybackState;
use crate::CurrentTheme;

#[derive(Debug)]
pub struct Pane {
    pub tile_id: Option<TileId>,
    pub nr: String,
    pub rect: Option<egui::Rect>,
}

pub struct TreeBehavior<'a> {
    pub current_theme: &'a CurrentTheme
}

impl<'a> egui_tiles::Behavior<Pane> for TreeBehavior<'a> {
    fn tab_bar_color(&self, visuals: &Visuals) -> Color32 {
        Color32::TRANSPARENT
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
            ..Default::default()
        }
    }
}

#[derive(Resource, Default)]
pub struct EguiTreeIdentities {
    pub code_tile: Option<TileId>,
    pub graph_tile: Option<TileId>,
    pub traces_tile: Option<TileId>,
}

#[derive(Resource)]
pub struct EguiTree {
    pub tree: egui_tiles::Tree<Pane>,
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
            tiles.insert_pane(gen_pane(String::from("Graph"))),
            // tiles.insert_pane(gen_pane(String::from("Traces"))),
        ];
        let root = tiles.insert_tab_tile(tabs);

        EguiTree {
            tree: egui_tiles::Tree::new("my_tree", root, tiles),
        }
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
    pub(crate) file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,

    /// Retain thread handle for the Chidori runtime
    pub(crate) background_thread: Mutex<Option<JoinHandle<()>>>,

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