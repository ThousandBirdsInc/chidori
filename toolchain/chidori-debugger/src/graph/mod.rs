//! Graph visualization module for the Chidori debugger.
//! 
//! This module provides a complete graph visualization system built on Bevy for displaying
//! execution graphs. It includes camera controls, node/edge rendering, user interaction,
//! layout algorithms, and UI components to visualize and navigate through execution traces.

use bevy::app::{App, Update};
use bevy::prelude::*;
use bevy::prelude::{in_state, IntoSystemConfigs, OnEnter, OnExit};
use bevy::asset::embedded_asset;
use crate::util::despawn_screen;
use crate::GameState;
use crate::application::{ChidoriState, EguiTree, EguiTreeIdentities};
use crate::graph::types::*;
use crate::{bevy_egui, CurrentTheme, RENDER_LAYER_GRAPH_VIEW};

pub mod types;
pub mod camera;
pub mod input;
pub mod layout;
pub mod rendering;
pub mod materials;
pub mod interaction;
pub mod contours;
pub mod ui;

pub use types::*;



pub fn graph_plugin(app: &mut App) {
    embedded_asset!(app, "rounded_rect.wgsl");
    app.init_resource::<NodeIdToEntity>()
        .init_resource::<EdgePairIdToEntity>()
        .init_resource::<SelectedEntity>()
        .init_resource::<InteractionLock>()
        .add_plugins(materials::MaterialPlugin::<materials::RoundedRectMaterial>::default())
        .add_systems(OnEnter(crate::GameState::Graph), setup::graph_setup)
        .add_systems(
            OnExit(crate::GameState::Graph),
            despawn_screen::<OnGraphScreen>,
        )
        .add_systems(
            Update,
            (
                input::keyboard_navigate_graph,
                rendering::compute_egui_transform_matrix,
                camera::mouse_pan,
                camera::set_camera_viewports,
                camera::update_minimap_camera_configuration,
                camera::update_trace_space_to_camera_configuration,
                camera::camera_follow_selection_head,
                interaction::node_cursor_handling,
                camera::touchpad_gestures,
                rendering::update_graph_system_renderer.after(camera::mouse_scroll_events),
                layout::update_graph_system_data_structures,
                camera::my_cursor_system,
                camera::mouse_scroll_events,
                input::mouse_over_system,
                camera::enforce_tiled_viewports.after(crate::application::tree::maintain_egui_tree_identities),
                materials::update_cursor_materials,
                materials::update_node_materials,
                ui::ui_window,
                contours::render_graph_grouping
            )
                .run_if(in_state(GameState::Graph)),
        );
}

pub mod setup {
    use super::*;
    use bevy::math::{vec2, vec3, Vec3};
    use bevy::prelude::*;
    use bevy::render::view::{NoFrustumCulling, RenderLayers};
    use bevy::render::camera::{Viewport};
    use bevy::window::{PrimaryWindow};
    use bevy::render::render_resource::Shader;
    use crate::{RENDER_LAYER_GRAPH_MINIMAP, RENDER_LAYER_GRAPH_VIEW};
    use crate::application::ChidoriState;

    pub fn graph_setup(
        windows: Query<&Window>,
        mut commands: Commands,
        execution_graph: Res<ChidoriState>,
        mut meshes: ResMut<Assets<Mesh>>,
        mut materials_standard: ResMut<Assets<StandardMaterial>>,
        mut materials_custom: ResMut<Assets<crate::graph::materials::RoundedRectMaterial>>,
        asset_server: Res<AssetServer>,
    ) {
        let window = windows.single();
        let scale_factor = window.scale_factor();

        let cursor_selection_material = materials_custom.add(crate::graph::materials::RoundedRectMaterial {
            width: 1.0,
            height: 1.0,
            color_texture: None,
            base_color: Vec4::new(0.565, 1.00, 0.882, 0.3),
            alpha_mode: AlphaMode::Blend,
        });

        let cursor_head_material = materials_custom.add(crate::graph::materials::RoundedRectMaterial {
            width: 1.0,
            height: 1.0,
            color_texture: None,
            base_color: Vec4::new(0.882, 0.00392, 0.357, 0.8),
            alpha_mode: AlphaMode::Blend,
        });

        // Main camera
        commands.spawn((
            Camera3dBundle {
                camera: Camera {
                    order: 2,
                    clear_color: ClearColorConfig::Custom(Color::rgba(0.035, 0.035, 0.043, 1.0)),
                    ..default()
                },
                projection: OrthographicProjection {
                    scale: 1.0,
                    near: -10000.0,
                    far: 10000.0,
                    ..default()
                }.into(),
                ..default()
            },
            OnGraphScreen,
            GraphMainCamera,
            CameraState { state: CameraStateValue::LockedOnExecHead },
            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
        ));

        // Minimap camera
        commands.spawn((
            Camera3dBundle {
                transform: Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::ZERO, Vec3::Y),
                camera: Camera {
                    order: 3,
                    clear_color: ClearColorConfig::Custom(Color::rgba(0.1, 0.1, 0.1, 1.0)),
                    viewport: Some(Viewport {
                        physical_position: UVec2::new((window.width() * scale_factor) as u32 - (300 * scale_factor as u32), 0),
                        physical_size: UVec2::new((300 * scale_factor as u32), (window.height() * scale_factor) as u32),
                        ..default()
                    }),
                    ..default()
                },
                projection: OrthographicProjection {
                    scale: 40.0,
                    ..default()
                }.into(),
                ..default()
            },
            OnGraphScreen,
            GraphMinimapCamera,
            CameraState { state: CameraStateValue::LockedOnExecHead },
            RenderLayers::from_layers(&[RENDER_LAYER_GRAPH_VIEW, RENDER_LAYER_GRAPH_MINIMAP])
        ));

        // Minimap viewport background
        commands.spawn((
            PbrBundle {
                mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))).into(),
                material: materials_standard.add(Color::hsla(3.0, 1.0, 1.0, 0.1)),
                transform: Transform::from_xyz(0.0, -50.0, -100.0).with_scale(vec3(100000.0, 100000.0, 1.0)),
                ..default()
            },
            RenderLayers::layer(RENDER_LAYER_GRAPH_MINIMAP),
            NoFrustumCulling,
            OnGraphScreen,
        ));

        let _ = commands.spawn((
            MaterialMeshBundle {
                mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
                material: cursor_selection_material.clone(),
                transform: Transform::from_xyz(0.0, 5.0, -3.0),
                ..Default::default()
            },
            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
            ExecutionSelectionCursor,
            OnGraphScreen
        ));

        let _ = commands.spawn((
            MaterialMeshBundle {
                mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
                material: cursor_head_material,
                transform: Transform::from_xyz(0.0, 0.0, -2.0),
                ..Default::default()
            },
            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
            ExecutionHeadCursor,
            OnGraphScreen
        ));

        use std::collections::HashMap;
        use petgraph::stable_graph::StableGraph;
        let dataset = StableGraph::new();
        let node_ids = HashMap::new();
        commands.spawn((CursorWorldCoords(vec2(0.0, 0.0)), OnGraphScreen));
        commands.insert_resource(GraphResource {
            execution_graph: dataset,
            group_dependency_graph: Default::default(),
            hash_graph: crate::graph::layout::hash_graph(&execution_graph.execution_graph),
            node_ids,
            node_dimensions: Default::default(),
            grouped_tree: Default::default(),
            is_active: false,
            is_layout_dirty: true,
            layout_graph: None,
        });
    }
} 