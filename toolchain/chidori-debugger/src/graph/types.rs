//! Type definitions and data structures for the graph visualization system.
//! 
//! This file defines core types, resources, and components used throughout the graph module,
//! including graph resources that hold execution data, component markers for entities,
//! and structures for managing node selection and interaction state.

use bevy::prelude::*;
use chidori_core::execution::execution::execution_graph::{ChronologyId, ExecutionNodeId};
use dashmap::DashMap;
use petgraph::stable_graph::StableGraph;
use std::collections::HashMap;
use crate::vendored::tidy_tree::TreeGraph;
use crate::bevy_egui::EguiRenderTarget;

#[derive(Resource, Default)]
pub struct SelectedEntity {
    pub id: Option<Entity>,
}

#[derive(Resource)]
pub struct GraphResource {
    pub execution_graph: StableGraph<ChronologyId, ()>,
    pub group_dependency_graph: StableGraph<ChronologyId, ()>,
    pub hash_graph: u64,
    pub node_ids: HashMap<ChronologyId, petgraph::prelude::NodeIndex>,
    pub node_dimensions: DashMap<ChronologyId, (f32, f32)>,
    pub grouped_tree: HashMap<ChronologyId, StableGraph<ChronologyId, ()>>,
    pub is_active: bool,
    pub layout_graph: Option<TreeGraph>,
    pub is_layout_dirty: bool
}

#[derive(Component)]
pub struct GraphIdx {
    pub loading: bool,
    pub execution_id: ExecutionNodeId,
    pub id: usize,
    pub is_hovered: bool,
    pub is_selected: bool
}

#[derive(Component)]
pub struct GraphIdxPair {
    pub source: usize,
    pub target: usize,
}

#[derive(Component, Default)]
pub struct CursorWorldCoords(pub Vec2);

#[derive(Component, Default)]
pub struct GraphMinimapViewportIndicator;

#[derive(Component, Default)]
pub struct GraphMainCamera;

#[derive(Component, Default)]
pub struct GraphMain2dCamera;

#[derive(Component, Default)]
pub struct GraphMinimapCamera;

pub enum CameraStateValue {
    LockedOnSelection,
    LockedOnExecHead,
    Free(f32, f32)
}

#[derive(Component)]
pub struct CameraState {
    pub state: CameraStateValue
}

#[derive(Default)]
pub enum InteractionLockValue {
    Panning,
    #[default]
    None
}

#[derive(Resource, Default)]
pub struct InteractionLock {
    pub inner: InteractionLockValue
}

#[derive(Resource, Default)]
pub struct SelectedNode(pub Option<petgraph::prelude::NodeIndex>);

#[derive(Default)]
pub struct KeyboardNavigationState {
    pub last_move: f32,
    pub move_cooldown: f32,
}

#[derive(Resource, Default)]
pub struct NodeIdToEntity {
    pub mapping: HashMap<petgraph::prelude::NodeIndex, Entity>,
}

#[derive(Resource, Default)]
pub struct EdgePairIdToEntity {
    pub mapping: HashMap<(usize, usize), Entity>,
}

#[derive(Component)]
pub struct ExecutionHeadCursor;

#[derive(Component)]
pub struct ExecutionSelectionCursor;

#[derive(Component)]
pub struct OnGraphScreen;

#[derive(Default)]
pub struct NodeResourcesCache {
    pub matched_strings_in_resource: HashMap<ChronologyId, Vec<(String, Vec<String>)>>,
    pub image_texture_cache: HashMap<String, egui::TextureHandle>
} 