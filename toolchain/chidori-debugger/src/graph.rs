use crate::chidori::{ChidoriState, EguiTree, EguiTreeIdentities};
use crate::tidy_tree::{Layout, Orientation, TidyLayout, TreeGraph};
use crate::util::{despawn_screen, egui_render_cell_function_evaluation, egui_render_cell_read, serialized_value_to_json_value};
use crate::{bevy_egui, chidori, util, CurrentTheme, GameState, Theme, RENDER_LAYER_GRAPH_MINIMAP, RENDER_LAYER_GRAPH_VIEW, RENDER_LAYER_TRACE_MINIMAP, RENDER_LAYER_TRACE_TEXT, RENDER_LAYER_TRACE_VIEW};
use bevy::app::{App, Update};
use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::input::touchpad::TouchpadMagnify;
use bevy::math::{vec2, vec3, Vec3};
use bevy::prelude::*;
use bevy::prelude::{
    default, in_state, Assets, Circle, Color, Commands, Component,
    IntoSystemConfigs, Mesh, OnEnter, OnExit, ResMut, Transform,
};
use bevy::render::render_resource::{AsBindGroup, Extent3d, ShaderRef, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::{NoFrustumCulling, RenderLayers};
use bevy::tasks::futures_lite::StreamExt;
use bevy::utils::petgraph::stable_graph::GraphIndex;
use bevy::window::{PrimaryWindow, WindowResized};
use egui::{Color32, Context, Frame, Margin, Order, Pos2, Rgba, RichText, Stroke, TextureHandle, Ui};
use crate::bevy_egui::{EguiContext, EguiContexts, EguiManagedTextures, EguiRenderOutput, EguiRenderTarget};
use egui;
use bevy_rapier2d::geometry::Collider;
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use bevy_rapier2d::prelude::*;
use chidori_core::execution::execution::execution_graph::{ChronologyId, ExecutionNodeId};
use chidori_core::execution::execution::ExecutionState;
use num::ToPrimitive;
use petgraph::data::DataMap;
use petgraph::prelude::{Dfs, NodeIndex, StableGraph};
use petgraph::visit::{IntoNeighborsDirected, Walker};
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::fmt::format;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use bevy::asset::embedded_asset;
use bevy::render::camera::{ScalingMode, Viewport};
use bevy::render::render_asset::RenderAssetUsages;
use crate::bevy_prototype_lyon::entity::Path;
use crate::bevy_prototype_lyon::path::PathBuilder;
use crate::bevy_prototype_lyon::prelude::{GeometryBuilder, ShapeBundle};
use crate::bevy_prototype_lyon::shapes;
use dashmap::DashMap;
use egui_extras::syntax_highlighting::CodeTheme;
use egui_json_tree::JsonTree;
use egui_tiles::Tile;
use image::{DynamicImage, ImageBuffer, RgbImage, RgbaImage};
use petgraph::{Graph, Outgoing};
use chidori_core::execution::execution::execution_state::{CloseReason, EnclosedState, ExecutionStateErrors};
use uuid::Uuid;
use chidori_core::execution::primitives::serialized_value::RkyvSerializedValue;
use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
use crate::bevy_prototype_lyon::draw::Fill;
use crate::graph_range_collector::{ElementDimensions, RangeCollector, StateRange};
use crate::tree_grouping::group_tree;

#[derive(Resource, Default)]
struct SelectedEntity {
    id: Option<Entity>,
}

#[derive(Resource)]
struct GraphResource {
    execution_graph: StableGraph<ChronologyId, ()>,
    group_dependency_graph: StableGraph<ChronologyId, ()>,
    hash_graph: u64,
    node_ids: HashMap<ChronologyId, NodeIndex>,
    node_dimensions: DashMap<ChronologyId, (f32, f32)>,
    grouped_tree: HashMap<ChronologyId, StableGraph<ChronologyId, ()>>,
    is_active: bool,
    layout_graph: Option<TreeGraph>,
    is_layout_dirty: bool
}


#[derive(Component)]
struct GraphIdx {
    loading: bool,
    execution_id: ExecutionNodeId,
    id: usize,
    is_hovered: bool,
    is_selected: bool

}

#[derive(Component)]
struct GraphIdxPair {
    source: usize,
    target: usize,
}

#[derive(Component, Default)]
struct CursorWorldCoords(Vec2);

#[derive(Component, Default)]
struct GraphMinimapViewportIndicator;

#[derive(Component, Default)]
struct GraphMainCamera;

#[derive(Component, Default)]
struct GraphMain2dCamera;

#[derive(Component, Default)]
struct GraphMinimapCamera;

enum CameraStateValue {
    LockedOnSelection,
    LockedOnExecHead,
    Free(f32, f32)
}

#[derive(Component)]
struct CameraState {
    state: CameraStateValue
}

#[derive(Default)]
enum InteractionLockValue {
    Panning,
    #[default]
    None
}

#[derive(Resource, Default)]
struct InteractionLock {
    inner: InteractionLockValue
}


// TODO: support graph traversal by id in the graph

#[derive(Resource, Default)]
struct SelectedNode(Option<NodeIndex>);


#[derive(Default)]
struct KeyboardNavigationState {
    last_move: f32,
    move_cooldown: f32,
}

fn keyboard_navigate_graph(
    time: Res<Time>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen> , With<GraphMainCamera>, Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>)>,
    mut graph_res: ResMut<GraphResource>,
    execution_graph: Res<ChidoriState>,
    mut selected_node: Local<SelectedNode>,
    mut node_query: Query<(Entity, &mut Transform, &GraphIdx)>,
    mut keyboard_nav_state: Local<KeyboardNavigationState>,
    mut selected_entity: ResMut<SelectedEntity>,
) {
    if !graph_res.is_active {
        return;
    }
    // Add a cooldown to prevent too rapid movement
    if time.elapsed_seconds() - keyboard_nav_state.last_move < keyboard_nav_state.move_cooldown {
        return;
    }
    let (_, _, mut camera_state) = q_camera.single_mut();
    let current_node = if let Some(node) = selected_node.0 {
        node
    } else {
        // If no node is selected, select the first node
        if let Some(node) = graph_res.execution_graph.node_indices().next() {
            let head = execution_graph.current_execution_head;
            let mut is_execution_head = false;
            node_query
                .iter()
                .for_each(|(e, node_transform, graph_idx)| {
                    if graph_idx.execution_id == head {
                        is_execution_head = true;
                        selected_entity.id = Some(e);
                    }
                });
            keyboard_nav_state.last_move = time.elapsed_seconds();
            keyboard_nav_state.move_cooldown = 0.1;
            camera_state.state = CameraStateValue::LockedOnSelection;
            if !is_execution_head {
                selected_node.0 = Some(node);
            } else {
                if let Some(head) = graph_res.node_ids.get(&head) {
                    selected_node.0 = Some(*head);
                }
                return;
            }
            node
        } else {
            return; // No nodes in the graph
        }
    };


    let mut new_selection = None;

    if keyboard_input.just_pressed(KeyCode::ArrowUp) {
        // Move to parent
        new_selection = graph_res.execution_graph
            .neighbors_directed(current_node, petgraph::Direction::Incoming)
            .next();
    } else if keyboard_input.just_pressed(KeyCode::ArrowDown) {
        // Move to first child
        new_selection = graph_res.execution_graph
            .neighbors_directed(current_node, petgraph::Direction::Outgoing)
            .next();
    } else if keyboard_input.just_pressed(KeyCode::ArrowLeft) {
        // Move to previous sibling
        if let Some(parent) = graph_res.execution_graph.neighbors_directed(current_node, petgraph::Direction::Incoming).next() {
            let siblings: Vec<_> = graph_res.execution_graph.neighbors_directed(parent, petgraph::Direction::Outgoing).collect();
            if let Some(current_index) = siblings.iter().position(|&node| node == current_node) {
                new_selection = siblings.get(current_index.checked_sub(1).unwrap_or(siblings.len() - 1)).cloned();
            }
        }
    } else if keyboard_input.just_pressed(KeyCode::ArrowRight) {
        // Move to next sibling
        if let Some(parent) = graph_res.execution_graph.neighbors_directed(current_node, petgraph::Direction::Incoming).next() {
            let siblings: Vec<_> = graph_res.execution_graph.neighbors_directed(parent, petgraph::Direction::Outgoing).collect();
            if let Some(current_index) = siblings.iter().position(|&node| node == current_node) {
                new_selection = siblings.get((current_index + 1) % siblings.len()).cloned();
            }
        }
    }


    if let Some(new_node) = new_selection {
        selected_node.0 = Some(new_node);
        keyboard_nav_state.last_move = time.elapsed_seconds();
        keyboard_nav_state.move_cooldown = 0.1;
        camera_state.state = CameraStateValue::LockedOnSelection;

        // Update the transform of the selected node (e.g., to highlight it)
        let node = &graph_res.execution_graph[new_node];
        node_query
            .iter()
            .for_each(|(e, node_transform, graph_idx)| {
                if graph_idx.execution_id == *node {
                    selected_entity.id = Some(e);
                }
            });
    }
}



fn update_minimap_camera_configuration(
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnGraphScreen>, With<GraphMinimapCamera>)>,
) {
    let (projection, _) = camera.single_mut();
    let (scale) = match projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { (&mut o.scaling_mode) }
    };
    // camera_transform.translation.y = -trace_space.max_vertical_extent / 2.0;
    // *scale = ScalingMode::Fixed {
    //     width: crate::traces::CAMERA_SPACE_WIDTH,
    //     height: trace_space.max_vertical_extent,
    // };
}

fn update_trace_space_to_camera_configuration(
    mut main_camera: Query<(&mut Projection, &mut Transform), (With<GraphMainCamera>, Without<GraphMinimapCamera>)>,
    mut minimap_viewport_indicator: Query<(&mut Transform), (With<GraphMinimapViewportIndicator>, Without<GraphMainCamera>, Without<GraphMinimapCamera>)>,
) {

    let (main_projection, mut main_camera_transform) = main_camera.single_mut();

    let main_projection = match main_projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { o }
    };

    let main_viewport_width = main_projection.area.width();
    let main_viewport_height = main_projection.area.height();

    minimap_viewport_indicator.iter_mut().for_each(|mut transform| {
        transform.translation.x = main_camera_transform.translation.x;
        transform.translation.y = main_camera_transform.translation.y;
        transform.scale.x = main_viewport_width;
        transform.scale.y = main_viewport_height;
    });
}

fn set_camera_viewports(
    windows: Query<&Window>,
    mut resize_events: EventReader<WindowResized>,
    mut minimap_camera: Query<&mut Camera, (With<GraphMinimapCamera>, Without<GraphMainCamera>)>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();
    // let minimap_offset = crate::traces::MINIMAP_OFFSET * scale_factor as u32;
    // let minimap_height = (crate::traces::MINIMAP_HEIGHT as f32 * scale_factor) as u32;
    // let minimap_height_and_offset = crate::traces::MINIMAP_HEIGHT_AND_OFFSET * scale_factor as u32;
    let mut minimap_camera = minimap_camera.single_mut();

    // We need to dynamically resize the camera's viewports whenever the window size changes
    // so then each camera always takes up half the screen.
    // A resize_event is sent when the window is first created, allowing us to reuse this system for initial setup.
    for _ in resize_events.read() {
        minimap_camera.viewport = Some(Viewport {
            physical_position: UVec2::new((window.width() * scale_factor) as u32 - (300 * scale_factor as u32), 0),
            physical_size: UVec2::new((300 * scale_factor as u32), (window.height() * scale_factor) as u32),
            ..default()
        });
    }
}

fn mouse_pan(
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen>, Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>)>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion_evr: EventReader<MouseMotion>,
    // node_query: Query<
    //     (Entity, &Transform, &GraphIdx, &EguiRenderTarget),
    //     (With<GraphIdx>, Without<GraphIdxPair>),
    // >,
    // rapier_context: Res<RapierContext>,

    graph_resource: Res<GraphResource>,
) {
    if !graph_resource.is_active {
        return;
    }

    let (projection, mut camera_transform, mut camera_state) = q_camera.single_mut();
    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };
    if buttons.pressed(MouseButton::Left) {
        let mut camera_x = camera_transform.translation.x;
        let mut camera_y = camera_transform.translation.y;
        for ev in motion_evr.read() {
            camera_x -= ev.delta.x * projection.scale;
            camera_y += ev.delta.y * projection.scale;
        }
        camera_state.state = CameraStateValue::Free(camera_x, camera_y);
    }
}



fn mouse_scroll_events(
    graph_resource: Res<GraphResource>,
    mut scroll_evr: EventReader<MouseWheel>,
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen> , Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>, Without<GraphMain2dCamera>)>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    q_mycoords: Query<&CursorWorldCoords, With<OnGraphScreen>>,
) {
    if !graph_resource.is_active {
        return;
    }

    let (projection, mut camera_transform, mut camera_state) = q_camera.single_mut();
    let mut coords = q_mycoords.single();

    if keyboard_input.just_pressed(KeyCode::Enter) {
        camera_state.state = CameraStateValue::LockedOnExecHead;
        return;
    }

    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };


    let mut camera_x = camera_transform.translation.x;
    let mut camera_y = camera_transform.translation.y;
    for ev in scroll_evr.read() {
        if keyboard_input.pressed(KeyCode::SuperLeft) {
            let zoom_base = (projection.scale + ev.y).clamp(1.0, 1000.0);
            let zoom_factor = zoom_base / projection.scale;
            camera_x = coords.0.x - zoom_factor * (coords.0.x - camera_transform.translation.x);
            camera_y = coords.0.y - zoom_factor * (coords.0.y - camera_transform.translation.y);
            camera_state.state = CameraStateValue::Free(camera_x, camera_y);
            projection.scale = zoom_base;
            // apply immediately to prevent jitter
            camera_transform.translation.y = camera_y;
            camera_transform.translation.x = camera_x;
        } else {
            camera_x -= ev.x * projection.scale;
            camera_y += (ev.y * 2.0) * projection.scale;
            camera_state.state = CameraStateValue::Free(camera_x, camera_y);
        }
    }
    // if !keyboard_input.pressed(KeyCode::SuperLeft) {
    //     camera_state.state = CameraStateValue::Free(camera_x, camera_y);
    // }

}

fn touchpad_gestures(
    mut q_camera: Query<(&mut Projection, &GlobalTransform), (With<OnGraphScreen>, Without<GraphMinimapCamera>)>,
    mut evr_touchpad_magnify: EventReader<TouchpadMagnify>,
) {
    let (projection, _) = q_camera.single_mut();
    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };
    for ev_magnify in evr_touchpad_magnify.read() {
        projection.scale -= ev_magnify.0;
    }
}


fn compute_egui_transform_matrix(
    mut q_egui_render_target: Query<(&mut EguiRenderTarget, &Transform), (With<EguiRenderTarget>, Without<Window>)>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Projection, &Camera, &GlobalTransform), (Without<GraphMinimapCamera>,  With<OnGraphScreen>)>,
) {
    let (_, camera, camera_transform) = q_camera.single();
    let window = q_window.single();
    let scale_factor = window.scale_factor();
    let viewport_pos = if let Some(viewport) = &camera.viewport {
        Vec2::new(viewport.physical_position.x as f32 / scale_factor, viewport.physical_position.y as f32 / scale_factor)
    } else {
        Vec2::ZERO
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    // Transform from the viewport offset into the world coordinates
    let Some(world_cursor_pos) = camera
        .viewport_to_world(camera_transform, cursor - viewport_pos)
        .map(|r| r.origin.truncate()) else {
        return;
    };

    for (mut egui_render_target , element_transform) in q_egui_render_target.iter_mut() {

        // Translate the element then revert the camera position relative to it
        let world_space_to_local_space = (
            // Mat4::from_translation(Vec3::new(0.0, (camera_transform.translation().y -element_transform.translation.y) * 2.0, 0.0))
                 Mat4::from_translation(vec3(element_transform.scale.x * -0.5, element_transform.scale.y * -0.5, 0.0))
                * Mat4::from_translation(element_transform.translation)
        ).inverse();

        let mut local_cursor_pos = world_space_to_local_space
            .transform_point3(world_cursor_pos.extend(0.0))
            .truncate();

        local_cursor_pos.y = element_transform.scale.y - local_cursor_pos.y;

        // let Some(screen_cursor_pos) = camera
        //     .world_to_viewport(camera_transform, world_cursor_pos.extend(0.0)) else {
        //     return;
        // };


        egui_render_target.cursor_position = Some(local_cursor_pos);
    }
}


fn my_cursor_system(
    mut q_mycoords: Query<&mut CursorWorldCoords, With<OnGraphScreen>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Camera, &GlobalTransform), (With<OnGraphScreen>, With<GraphMainCamera>, Without<GraphMinimapCamera>)>,
) {
    let mut coords = q_mycoords.single_mut();
    let (camera, camera_transform) = q_camera.single();
    let window = q_window.single();
    let scale_factor = window.scale_factor();
    let viewport_pos = if let Some(viewport) = &camera.viewport {
        vec2(viewport.physical_position.x as f32 / scale_factor , viewport.physical_position.y as f32 / scale_factor)
    } else {
        Vec2::ZERO
    };
    if let Some(world_position) = window.cursor_position()
        .and_then(|cursor| {
            let adjusted_cursor = cursor - viewport_pos;
            camera.viewport_to_world(camera_transform, adjusted_cursor)
        })
        .map(|ray| ray.origin.truncate())
    {
        // Adjust according to the ratio of our actual window size and our scaling independently of it
        coords.0 = world_position;
    }
}

fn egui_execution_state(
    ui: &mut Ui,
    mut internal_state: &mut ChidoriState,
    execution_state: &ExecutionState,
    current_theme: &Theme
) {
    ui.vertical(|ui| {
        ui.label("Evaluated:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.vertical(|ui| {
                if internal_state.debug_mode {
                    ui.label(format!("Chronology Id: {:?}", execution_state.chronology_id));
                    ui.label(format!("Chronology Parent Id: {:?}", execution_state.parent_state_chronology_id));
                    ui.label(format!("Resolving Execution Node Id: {:?}", execution_state.resolving_execution_node_state_id));
                    ui.label(format!("Enclosed State: {:?}", execution_state.evaluating_enclosed_state));
                    ui.label(format!("Function Name: {:?}", execution_state.evaluating_fn));
                    ui.label(format!("Operation Id: {:?}", execution_state.evaluating_operation_id));
                }
                if let Some(evaluating_name) = execution_state.evaluating_name.as_ref() {
                    ui.label(format!("Cell Name: {:?}", evaluating_name));
                }
                egui_render_cell_function_evaluation(ui, execution_state);
                if !execution_state.state.is_empty() {
                    ui.label("Output:");
                    // let mut frame = egui::Frame::default()
                    //     .fill(current_theme.background).stroke(current_theme.card_border).inner_margin(16.0).rounding(6.0).begin(ui);
                    // {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        for (key, value) in execution_state.state.iter() {
                            if execution_state.fresh_values.contains(key) {
                                match &value.output.clone() {
                                    Ok(o) => {
                                        let _ = JsonTree::new(format!("{:?}", key), &serialized_value_to_json_value(&o))
                                            // .default_expand(DefaultExpand::SearchResults(&self.search_input))
                                            .show(ui);
                                    }
                                    Err(e) => {
                                        ui.label(format!("{:?}", e));
                                    }
                                }
                            }
                        }
                    });
                }
            })
        });



        if let Some(cell) = &execution_state.evaluating_cell {
            egui::CollapsingHeader::new("Cell Definition")
                .show(ui, |ui| {
                    egui_render_cell_read(ui, cell, execution_state);
                });
        }

        if internal_state.debug_mode {
            if !execution_state.stack.is_empty() {
                ui.label("Exec Stack:");
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        for item in &execution_state.stack {
                            ui.label(format!("{:?}", item));
                        }
                    })
                });
            }
            if let Some(args) = &execution_state.evaluating_arguments {
                ui.label("Evaluating With Arguments");
                let _ = JsonTree::new(format!("evaluating_args"), &serialized_value_to_json_value(&args))
                    // .default_expand(DefaultExpand::SearchResults(&self.search_input))
                    .show(ui);
            }
        }

        if let Some((op_id, _)) = &execution_state.evaluated_mutation_of_cell {
            ui.label("Cell Mutation:");
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                // ui.vertical(|ui| {
                //     ui.label(format!("Operation Id: {:?}", execution_state.evaluating_id));
                // })
            });
            // egui_render_cell_read(ui, cell, execution_state);
            let mut code_theme = egui_extras::syntax_highlighting::CodeTheme::dark();
            crate::code::editable_chidori_cell_content(
                &mut internal_state,
                &current_theme,
                ui,
                &mut code_theme,
                *op_id);
        }


    });
}

fn camera_follow_selection_head(
    mut q_camera: Query<(&Camera, &mut Transform, &CameraState), (With<OnGraphScreen>,  With<GraphMainCamera>, Without<ExecutionSelectionCursor>, Without<GraphMinimapCamera>)>,
    mut execution_selection_query: Query<
        (Entity, &mut Transform),
        (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>),
    >,
    mut execution_head_cursor: Query<
        (Entity, &mut Transform),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>, Without<GraphMainCamera>),
    >,
) {
    let (_, mut camera_transform, camera_state) = q_camera.single_mut();
    let (_, mut t) = execution_head_cursor.single_mut();

    if matches!(camera_state.state, CameraStateValue::LockedOnExecHead) {
        camera_transform.translation.x = t.translation.x;
        camera_transform.translation.y = t.translation.y;
    }

    let (_, mut t) = execution_selection_query.single_mut();
    if matches!(camera_state.state, CameraStateValue::LockedOnSelection) {
        camera_transform.translation.x = t.translation.x;
        camera_transform.translation.y = t.translation.y;
    }

    if let CameraStateValue::Free(x, y) = camera_state.state {
        camera_transform.translation.x = x;
        camera_transform.translation.y = y;
    }
}

fn mouse_over_system(
    mut graph_resource: ResMut<GraphResource>,
    buttons: Res<ButtonInput<MouseButton>>,
    q_mycoords: Query<&CursorWorldCoords, With<OnGraphScreen>>,
    mut selected_entity: ResMut<SelectedEntity>,
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx, &mut EguiRenderTarget),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut gizmos: Gizmos,
    rapier_context: Res<RapierContext>,
) {
    if !graph_resource.is_active {
        return;
    }
    // https://docs.rs/bevy/latest/bevy/prelude/enum.CursorIcon.html
    let cursor = q_mycoords.single();

    for (_, _, mut gidx, mut egui_render_target) in node_query.iter_mut() {
        gidx.is_hovered = false;
        egui_render_target.is_focused = false;
    }

    gizmos
        .circle(
            Vec3::new(cursor.0.x, cursor.0.y, 0.0),
            Direction3d::Z,
            1.0,
            Color::YELLOW,
        )
        .segments(64);
    let point = Vec2::new(cursor.0.x, cursor.0.y);
    let filter = QueryFilter::default();
    rapier_context.intersections_with_point(point, filter, |entity| {
        if let Ok((_, _, mut gidx, mut egui_render_target)) = node_query.get_mut(entity) {
            gidx.is_hovered = true;
            egui_render_target.is_focused = true;

            if buttons.just_pressed(MouseButton::Left) {
                gidx.is_selected = true;
                selected_entity.id = Some(entity);
            }
        }

        true
    });

    // Deselect others
    for (entity, _, mut gidx, _) in node_query.iter_mut() {
        if Some(entity) != selected_entity.id {
            gidx.is_selected = false;
        }
    }
}

fn node_cursor_handling(
    selected_entity: Res<SelectedEntity>,
    mut execution_head_query: Query<
        (Entity, &mut Transform),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>),
    >,
    mut execution_selection_query: Query<
        (Entity, &mut Transform),
        (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>),
    >,
    mut node_query: Query<
        (Entity, &Transform, &GraphIdx),
        (With<GraphIdx>, Without<ExecutionHeadCursor>, Without<ExecutionSelectionCursor>),
    >,
    execution_graph: Res<crate::chidori::ChidoriState>,
) {
    node_query
        .iter_mut()
        .for_each(|(entity, node_transform, graph_idx)| {
            if execution_graph.current_execution_head == graph_idx.execution_id {
                let (_, mut t) = execution_head_query.single_mut();
                t.translation.x = node_transform.translation.x;
                t.translation.y = node_transform.translation.y;
                t.scale = node_transform.scale + 16.0;
                t.translation.z = -3.0;
            }
            if Some(entity) == selected_entity.id {
                let (_, mut t) = execution_selection_query.single_mut();
                t.translation.x = node_transform.translation.x;
                t.translation.y = node_transform.translation.y;
                t.scale = node_transform.scale + 10.0;
                t.translation.z = -2.0;
            }
        });
}

#[derive(Resource, Default)]
struct NodeIdToEntity {
    mapping: HashMap<NodeIndex, Entity>,
}

#[derive(Resource, Default)]
struct EdgePairIdToEntity {
    mapping: HashMap<(usize, usize), Entity>,
}

fn save_image_to_png(image: &Image) {
    let width = image.texture_descriptor.size.width;
    let height = image.texture_descriptor.size.height;
    let data = &image.data;
    let img_buffer = RgbaImage::from_raw(width, height, data.clone()).expect("Failed to create image buffer");
    img_buffer.save(std::path::Path::new("./outputimage.png")).expect("Failed to save image");
}

fn generate_noodle_path(elements: &[ElementDimensions], noodle_width: f32, nesting_depth: usize) -> (Path, Vec<ControlPoint>) {
    let left_offset = nesting_depth as f32 * noodle_width * -6.0 - 30.0;
    let mut path_builder = PathBuilder::new();
    let mut all_points = Vec::new();

    if elements.is_empty() {
        return (path_builder.build(), all_points);
    }

    // Sort elements by y position (top to bottom)
    let mut sorted_elements = elements.to_vec();
    sorted_elements.sort_by(|a, b| b.y.partial_cmp(&a.y).unwrap());

    // Find the topmost and bottommost points
    let highest_elem = sorted_elements.first().unwrap();
    let lowest_elem = sorted_elements.last().unwrap();

    let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, highest_elem, left_offset);

    // Helper function to calculate handles based on point distance
    fn calculate_handles(current: Vec2, next: Vec2, path_direction: f32) -> (Vec2, Vec2) {
        let path_direction = Vec2::new(0.0, path_direction);
        let distance = (next - current).length();
        let handle_length = distance / 3.0;
        let in_handle = -path_direction * handle_length;
        let out_handle = path_direction * handle_length;
        (out_handle, in_handle)
    }

    // Generate forward path points
    let mut forward_points = Vec::new();

    // Start point
    forward_points.push(ControlPoint::new(
        Vec2::new( left_x, top_y ),
        Vec2::new( left_x, top_y ),
        None,
        None
    ));

    forward_points.push(ControlPoint::new(
        Vec2::new( left_x, top_y ),
        Vec2::new( left_x, bottom_y ),
        None,
        None
    ));

    // Process each element for forward path
    for (i, window) in sorted_elements.windows(3).enumerate() {
        let (prev_left_x, prev_top_y, prev_bottom_y) = elem_anchors(noodle_width, &window[0], left_offset);
        let prev_bottom = Vec2::new(prev_left_x, prev_bottom_y);

        let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, &window[1], left_offset);

        let current_top = Vec2::new(left_x, top_y);
        let current_bottom = Vec2::new(left_x, bottom_y);
        let (out_handle, in_handle) = calculate_handles(prev_bottom, current_top, 1.0);

        if (i == 0) {
            forward_points.push(ControlPoint::new(
                prev_bottom,
                current_top,
                Some(in_handle),
                Some(out_handle)
            ));
        }

        forward_points.push(ControlPoint::new(
            current_top,
            current_bottom,
            None,
            None
        ));

        let (next_left_x, next_top_y, next_bottom_y) = elem_anchors(noodle_width, &window[2], left_offset);
        let next_top = Vec2::new(next_left_x, next_top_y);
        let (out_handle, in_handle) = calculate_handles(current_bottom, next_top, 1.0);
        forward_points.push(ControlPoint::new(
            current_bottom,
            next_top,
            Some(in_handle),
            Some(out_handle),
        ));
    }

    // Add control points for the solid edge of the last element
    let end_point = {
        let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, lowest_elem, left_offset);

        forward_points.push(ControlPoint::new(
            Vec2::new( left_x, top_y ),
            Vec2::new( left_x, top_y ),
            None,
            None
        ));

        // End point of forward path
        forward_points.push(ControlPoint::new(
            Vec2::new( left_x, top_y ),
            Vec2::new( left_x, bottom_y ),
            None,
            None
        ));

        Vec2::new( left_x, bottom_y )
    };

    // Draw the forward path
    path_builder.move_to(forward_points[0].end_position);
    for current in forward_points.iter() {
        if let (Some(in_handle), Some(out_handle)) = (current.in_handle, current.out_handle) {
            path_builder.cubic_bezier_to(
                current.start_position + in_handle,
                current.end_position + out_handle,
                current.end_position
            );
        } else {
            path_builder.line_to(current.end_position);
        }
    }

    // Draw bottom semicircle
    path_builder.arc(
        end_point + Vec2::new(noodle_width/2.0, 0.0),
        Vec2::new(noodle_width/2.0, noodle_width/2.0),
        PI as f32,
        PI as f32
    );

    let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, lowest_elem, left_offset + noodle_width);

    // Generate return path points
    let mut return_points = Vec::new();
    return_points.push(ControlPoint::new(
        Vec2::new( left_x, bottom_y ),
        Vec2::new( left_x, bottom_y ),
        None,
        None
    ));

    return_points.push(ControlPoint::new(
        Vec2::new( left_x, bottom_y ),
        Vec2::new( left_x, top_y ),
        None,
        None
    ));


    let reversed_sorted_elements: Vec<_> = sorted_elements.iter().rev().collect();
    for (i, window) in reversed_sorted_elements.windows(3).enumerate() {
        let (prev_left_x, prev_top_y, prev_bottom_y) = elem_anchors(noodle_width, &window[0], left_offset + noodle_width);
        let prev_top = Vec2::new(prev_left_x, prev_top_y);

        let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, &window[1], left_offset + noodle_width);

        let current_top = Vec2::new(left_x, top_y);
        let current_bottom = Vec2::new(left_x, bottom_y);
        let (out_handle, in_handle) = calculate_handles(prev_top, current_bottom, -1.0);

        if i == 0 {
            return_points.push(ControlPoint::new(
                prev_top,
                current_bottom,
                Some(in_handle),
                Some(out_handle),
            ));
        }

        return_points.push(ControlPoint::new(
            current_bottom,
            current_top,
            None,
            None
        ));

        let (next_left_x, next_top_y, next_bottom_y) = elem_anchors(noodle_width, &window[2], left_offset + noodle_width);
        let next_bottom = Vec2::new(next_left_x, next_bottom_y);
        let (out_handle, in_handle) = calculate_handles(current_top, next_bottom, -1.0);
        return_points.push(ControlPoint::new(
            current_top,
            next_bottom,
            Some(in_handle),
            Some(out_handle),
        ));
    }

    let (left_x, top_y, bottom_y) = elem_anchors(noodle_width, highest_elem, left_offset + noodle_width);
    return_points.push(ControlPoint::new(
        Vec2::new( left_x, top_y ),
        Vec2::new( left_x, top_y ),
        None,
        None
    ));

    // Draw the return path
    for current in return_points.iter() {
        if let (Some(in_handle), Some(out_handle)) = (current.in_handle, current.out_handle) {
            path_builder.cubic_bezier_to(
                current.start_position + in_handle,
                current.end_position + out_handle,
                current.end_position
            );
        } else {
            path_builder.line_to(current.end_position);
        }
    }

    // Draw top semicircle and close total path
    let start_point = forward_points[0].start_position;
    path_builder.arc(
        start_point + Vec2::new(noodle_width/2.0, 0.0),
        Vec2::new(noodle_width/2.0, noodle_width/2.0),
        PI as f32,
        PI as f32
    );
    path_builder.close();

    // Combine all points in the correct order
    all_points.extend(forward_points);
    all_points.extend(return_points);

    (path_builder.build(), all_points)
}

fn elem_anchors(noodle_width: f32, prev: &ElementDimensions, left_offset: f32) -> (f32, f32, f32) {
    let prev_left_x = prev.x - prev.width / 2.0 - noodle_width + left_offset;
    let prev_top_y = prev.y + prev.height / 2.0 + noodle_width;
    let prev_bottom_y = prev.y - prev.height / 2.0 - noodle_width;
    (prev_left_x, prev_top_y, prev_bottom_y)
}

// Helper struct to store point and handle information
#[derive(Clone)]
struct ControlPoint {
    start_position: Vec2,
    end_position: Vec2,
    in_handle: Option<Vec2>,  // Handle for incoming curve
    out_handle: Option<Vec2>, // Handle for outgoing curve
}

impl ControlPoint {
    fn new(start_position: Vec2, end_position: Vec2, in_handle: Option<Vec2>, out_handle: Option<Vec2>) -> Self {
        Self {
            start_position,
            end_position,
            in_handle,
            out_handle,
        }
    }
}


fn generate_contour_path_for_range(range: &StateRange) -> (Path, Vec<ControlPoint>) {
    if range.elements.is_empty() {
        return (PathBuilder::new().build(), Vec::new());
    }

    generate_noodle_path(&range.elements, 3.0, range.nesting_depth)
}



fn get_color_for_depth(depth: usize) -> Color {
    // Define a palette of visually distinct colors
    const COLORS: &[Color] = &[
        Color::rgb(0.0, 0.7, 0.9),     // Cyan
        Color::rgb(0.9, 0.1, 0.1),     // Red
        Color::rgb(0.1, 0.8, 0.1),     // Green
        Color::rgb(0.9, 0.6, 0.1),     // Orange
        Color::rgb(0.6, 0.1, 0.9),     // Purple
        Color::rgb(0.9, 0.9, 0.1),     // Yellow
        Color::rgb(0.1, 0.1, 0.9),     // Blue
        Color::rgb(0.9, 0.1, 0.9),     // Magenta
    ];

    COLORS[depth % COLORS.len()]
}


fn render_graph_grouping(
    mut commands: Commands,
    mut graph_resource: ResMut<GraphResource>,
    mut chidori_state: ResMut<ChidoriState>,
    mut range_id_to_entity_id: Local<HashMap<(ChronologyId, ChronologyId), Entity>>,
    mut cached_collector: Local<Vec<StateRange>>,
    // Add query to modify existing entities
    mut existing_shapes: Query<(&mut Path, &mut Transform)>,
) {
    let execution_graph = &graph_resource.execution_graph;
    let tree_graph = generate_tree_layout(&execution_graph, &graph_resource.node_dimensions);

    // Main collection logic
    let mut collector = RangeCollector::new();

    // Create a map of node indices to dimensions for efficient lookup
    let mut dimensions_map = HashMap::new();

    // First pass: collect dimensions
    let mut topo = petgraph::visit::Topo::new(&graph_resource.execution_graph);
    while let Some(idx) = topo.next(&graph_resource.execution_graph) {
        if let Some(node) = &graph_resource.execution_graph.node_weight(idx) {
            if let Some((_, n)) = tree_graph.get_from_external_id(&idx.index()) {
                dimensions_map.insert(idx, ElementDimensions {
                    width: n.width.to_f32().unwrap(),
                    height: n.height.to_f32().unwrap(),
                    x: n.x.to_f32().unwrap(),
                    y: -n.y.to_f32().unwrap(),
                });
            }
        }
    }

    // Second pass: find Open states and collect paths
    let mut topo = petgraph::visit::Topo::new(&graph_resource.execution_graph);
    while let Some(idx) = topo.next(&graph_resource.execution_graph) {
        if let Some(chronology_id) = &graph_resource.execution_graph.node_weight(idx) {
            if let Some(state) = chidori_state.get_execution_state_at_id(&chronology_id) {
                if let EnclosedState::Open = state.evaluating_enclosed_state {
                    collector.collect_paths(
                        &graph_resource.execution_graph,
                        idx,
                        state.resolving_execution_node_state_id.clone(),
                        &dimensions_map,
                        &chidori_state
                    );
                }
            }
        }
    }

    collector.remove_implicitly_ended_ranges();
    collector.calculate_nesting_depths();

    let mut existing_ranges: HashSet<_> = range_id_to_entity_id.keys().cloned().collect();
    for range in collector.ranges {
        existing_ranges.remove(&range.id());
        if range.elements.len() <= 1 {
            continue;
        }

        let (path , _)= generate_contour_path_for_range(&range);
        let z_position = range.nesting_depth as f32 * -10.0;
        let color = get_color_for_depth(range.nesting_depth);

        if let Some(&entity) = range_id_to_entity_id.get(&range.id()) {
            // Update existing entity
            if let Ok((mut path_component, mut transform)) = existing_shapes.get_mut(entity) {
                *path_component = path;
                transform.translation.z = z_position;
            }
        } else {
            // Spawn new entity
            let entity = commands.spawn((
                ShapeBundle {
                    path,
                    transform: Transform::from_xyz(0.0, 0.0, z_position),
                    ..default()
                },
                crate::bevy_prototype_lyon::prelude::Fill::color(color),
                crate::bevy_prototype_lyon::prelude::Stroke::new(color, 10.0),
                RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
                OnGraphScreen
            ));
            range_id_to_entity_id.insert(range.id(), entity.id());
        }
    }
    for missing_range in existing_ranges {
        if let Some(entity ) = range_id_to_entity_id.remove(&missing_range) {
            commands.entity(entity).despawn_recursive();
        }

    }
}


fn update_graph_system_renderer(
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut commands: Commands,
    mut graph_resource: ResMut<GraphResource>,
    mut edge_pair_id_to_entity: ResMut<EdgePairIdToEntity>,
    mut node_id_to_entity: ResMut<NodeIdToEntity>,
    current_theme: Res<CurrentTheme>,
    mut node_query: Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut edge_query: Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
    mut images: ResMut<Assets<Image>>,
    mut materials_custom: ResMut<Assets<RoundedRectMaterial>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut chidori_state: ResMut<ChidoriState>,
    mut node_index_to_entity: Local<HashMap<usize, Entity>>,
    mut node_image_texture_cache: Local<HashMap<String, egui::TextureHandle>>,
) {
    // TODO: something in this logic is affecting the trace rendering
    if !graph_resource.is_active {
        return;
    }
    let window = q_window.single();


    // For each subgraph group
    // Grouping background
    if false {
        let cursor_selection_material = materials_custom.add(RoundedRectMaterial {
            width: 1.0,
            height: 1.0,
            color_texture: None,
            base_color: Vec4::new(0.565, 1.00, 0.882, 0.00),
            alpha_mode: AlphaMode::Blend,
        });
        let entity_selection_head = commands.spawn((
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
    }


    // Construct the tidy_tree from a topo traversal of the graph
    // TODO: grouping is currently unused
    let execution_graph = &graph_resource.execution_graph;
    let grouped_nodes = &graph_resource.grouped_tree;
    let group_dep_graph = &graph_resource.group_dependency_graph;
    // let mut group_layouts = HashMap::new();
    // dbg!(&grouped_nodes);
    for (id, group_graph) in grouped_nodes {
        let tree_layout = generate_tree_layout(&group_graph, &graph_resource.node_dimensions);
        // group_layouts.insert(id, tree_layout);
    }

    // TODO: traverse the group dep graph, allocating nodes

    if graph_resource.is_layout_dirty {
        let tree_graph = generate_tree_layout(&execution_graph, &graph_resource.node_dimensions);
        graph_resource.layout_graph = Some(tree_graph);
        graph_resource.is_layout_dirty = false;
    }
    let tree_graph = if let Some(tree_graph) = &graph_resource.layout_graph {
        tree_graph
    } else {
        panic!("Missing tree graph");
    };
    let mut flag_layout_is_dirtied = false;

    // Traverse the graph again, and render the elements of the graph based on their layout in the tidy_tree
    // This traverses the graph and then gets the position of the elements in the tree from their identity
    let mut topo = petgraph::visit::Topo::new(&graph_resource.execution_graph);
    while let Some(idx) = topo.next(&graph_resource.execution_graph) {
        if let Some(node) = &graph_resource.execution_graph.node_weight(idx) {
            let mut parents = &mut graph_resource
                .execution_graph
                .neighbors_directed(idx, petgraph::Direction::Incoming);

            // Get position of the node's parent
            let parent_pos = parents
                .next()
                .and_then(|parent| node_id_to_entity.mapping.get(&parent))
                .and_then(|entity| {
                    if let Ok((_, mut transform, _, _, _, _)) = node_query.get_mut(*entity) {
                        Some(transform.translation.truncate())
                    } else {
                        None
                    }
                }).unwrap_or(vec2(0.0, 0.0));

            if let Some((n_idx, n)) = tree_graph.get_from_external_id(&idx.index()) {
                // Create the appropriately sized egui render target texture
                let width = n.width.to_f32().unwrap();
                let height = n.height.to_f32().unwrap();
                let entity = node_id_to_entity.mapping.entry(idx).or_insert_with(|| {
                    // This is the texture that will be rendered to.
                    let (scale_factor, scaled_width, scaled_height, image) = create_egui_texture_image(window, width, height);
                    let image_handle = images.add(image);
                    let node_material = materials_custom.add(RoundedRectMaterial {
                        width: 1.0,
                        height: 1.0,
                        color_texture: Some(image_handle.clone()),
                        base_color: Vec4::new(0.0, 0.0, 0.0, 1.0),
                        alpha_mode: AlphaMode::Blend,
                    });

                    let entity = commands.spawn((
                        MaterialMeshBundle {
                            mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
                            material: node_material,
                            transform: Transform::from_xyz(parent_pos.x, parent_pos.y, -1.0).with_scale(vec3(width, height, 1.0)),
                            ..Default::default()
                        },
                        GraphIdx {
                            loading: false,
                            execution_id: **node,
                            id: idx.index(),
                            is_hovered: false,
                            is_selected: false,
                        },
                        EguiRenderTarget {
                            inner_physical_width: scaled_width,
                            inner_physical_height: scaled_height,
                            image: Some(image_handle),
                            inner_scale_factor: scale_factor,
                            ..default()
                        },
                        Sensor,
                        Collider::cuboid(0.5, 0.5),
                        RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
                        OnGraphScreen
                    ));
                    node_index_to_entity.insert(n.external_id, entity.id());
                    entity.id()
                });

                // Check if dimensions have changed for existing entries
                if let Ok((_, mut transform, _, _, mut egui_render_target, material_handle)) = node_query.get_mut(*entity) {
                    let current_image = egui_render_target.image.as_ref();
                    let dimensions_changed = current_image.map_or(true, |image| {
                        let texture = images.get(image).unwrap();
                        texture.texture_descriptor.size.width != (width * window.scale_factor()) as u32 ||
                            texture.texture_descriptor.size.height != (height * window.scale_factor()) as u32
                    });

                    if dimensions_changed {
                        // Create new image with updated dimensions
                        let (scale_factor, scaled_width, scaled_height, image) = create_egui_texture_image(window, width, height);
                        let image_handle = images.add(image);

                        // Create new EguiRenderTarget, this avoids issues swapping the image target underneath rendering
                        // which otherwise resulted in scissor rect errors.
                        let new_egui_render_target = EguiRenderTarget {
                            inner_physical_width: scaled_width,
                            inner_physical_height: scaled_height,
                            image: Some(image_handle.clone()),
                            inner_scale_factor: scale_factor,
                            ..default()
                        };

                        // Replace the old EguiRenderTarget with the new one
                        commands.entity(*entity).remove::<EguiRenderTarget>()
                            .insert(new_egui_render_target);

                        // Update material with new texture
                        let mut material = materials_custom.get_mut(material_handle).unwrap();
                        material.color_texture = Some(image_handle);

                        transform.scale.x = scaled_width as f32;
                        transform.scale.y = scaled_height as f32;
                    }
                }


                if let Ok((entity, mut transform, _, mut egui_ctx, _, _)) = node_query.get_mut(*entity) {
                    let egui_ctx = egui_ctx.into_inner();
                    let ctx = egui_ctx.get_mut();

                    // Position the node according to its tidytree layout
                    transform.translation = transform.translation.lerp(Vec3::new(n.x.to_f32().unwrap(), -n.y.to_f32().unwrap(), -1.0), 0.5);

                    // Draw text within these elements
                    egui_graph_node(&current_theme, &mut chidori_state, &mut node_image_texture_cache, node, entity, ctx);

                    let used_rect = ctx.used_rect();
                    let height = used_rect.height();
                    graph_resource.node_dimensions.insert(*node.clone(), (1000.0, height));
                    flag_layout_is_dirtied = true;
                    transform.scale = vec3(width, height, 1.0);
                }

                tree_graph.get_children(*n_idx).into_iter().for_each(|child| {
                    let child = &tree_graph.graph[child];
                    let parent_pos = if let Some(entity ) = node_index_to_entity.get(&n.external_id) {
                        if let Ok((_, mut transform, _, _, _, _)) = node_query.get(*entity) {
                            transform.translation.truncate()
                        } else {
                            return;
                        }
                    } else {
                        return;
                    };
                    let child_pos = if let Some(entity ) = node_index_to_entity.get(&child.external_id) {
                        if let Ok((_, mut transform, _, _, _, _)) = node_query.get(*entity) {
                            transform.translation.truncate()
                        } else {
                            return;
                        }
                    } else {
                        return;
                    };
                    let midpoint = (parent_pos + child_pos) / 2.0;
                    let distance = (parent_pos - child_pos).length();
                    let angle = (child_pos.y - parent_pos.y).atan2(child_pos.x - parent_pos.x);

                    let entity = edge_pair_id_to_entity.mapping.entry((n.external_id, child.external_id)).or_insert_with(|| {
                        let entity = commands.spawn((
                            PbrBundle {
                                mesh: meshes.add(Rectangle::new(1.0, 1.0)),
                                transform: Transform::from_xyz(midpoint.x, midpoint.y, -50.0).with_scale(vec3(distance, 3.0, 1.0)).with_rotation(Quat::from_rotation_z(-angle)),
                                material: materials.add(StandardMaterial {
                                    base_color: Color::hex("#ffffff").unwrap().into(),
                                    unlit: true,
                                    ..default()
                                }),
                                ..default()
                            },
                            GraphIdxPair{
                                source: n.external_id,
                                target: child.external_id,
                            },
                            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
                            OnGraphScreen ));
                        entity.id()
                    });


                    if let Ok((_, mut transform, _)) = edge_query.get_mut(*entity) {
                        transform.translation = vec3(midpoint.x, midpoint.y, -50.0);
                        transform.scale = vec3(distance, 3.0, 1.0);
                        transform.rotation = Quat::from_rotation_z(angle);
                    }

                });
            }
        }
    }

    if flag_layout_is_dirtied {
        graph_resource.is_layout_dirty = true;
    }
}

fn egui_graph_node(
    current_theme: &Res<CurrentTheme>,
    mut chidori_state: &mut ResMut<ChidoriState>,
    mut node_image_texture_cache: &mut HashMap<String, TextureHandle>,
    node: &&ChronologyId,
    entity: Entity,
    ctx: &mut Context
) {
    egui::Area::new(format!("{:?}", entity).into())
        .fixed_pos(Pos2::new(0.0, 0.0)).show(ctx, |ui| {
        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
        let mut frame = egui::Frame::default().fill(current_theme.theme.card).stroke(current_theme.theme.card_border)
            .inner_margin(16.0).rounding(6.0).begin(ui);
        {
            ui.set_min_width(800.0);
            ui.set_max_width(800.0);
            let mut ui = &mut frame.content_ui;
            let node1 = *node;
            let original_style = (*ui.ctx().style()).clone();

            let mut style = original_style.clone();
            // style.visuals.override_text_color = Some(Color32::BLACK);
            ui.set_style(style);


            if *node1 == chidori_core::uuid::Uuid::nil() {
                ui.horizontal(|ui| {
                    ui.label("Initialization...");
                    if chidori_state.debug_mode {
                        ui.label(node1.to_string());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.button(RichText::new("Revert to this State").color(Color32::from_hex("#dddddd").unwrap())).clicked() {
                            let _ = chidori_state.set_execution_id(*node1);
                        }
                    });
                });
            } else {
                if let Some(state) = chidori_state.get_execution_state_at_id(&node1) {
                    let state = &state;
                    if !matches!(state.evaluating_enclosed_state, EnclosedState::Open) {
                        ui.horizontal(|ui| {
                            if chidori_state.debug_mode {
                                ui.label(node1.to_string());
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if ui.button("Revert to this State").clicked() {
                                    info!("We would like to revert to {:?}", node1);
                                    let _ = chidori_state.set_execution_id(*node1);
                                }
                            });
                        });
                    }

                    match &state.evaluating_enclosed_state {
                        EnclosedState::Close(CloseReason::Error) => {
                            let mut frame = egui::Frame::default().fill(current_theme.theme.card).stroke(Stroke {
                                width: 0.5,
                                color: Color32::from_hex("#ff0000").unwrap(),
                            }).inner_margin(16.0).rounding(6.0).begin(ui);
                            {
                                let mut ui = &mut frame.content_ui;
                                ui.label("Error");
                                egui_execution_state(
                                    ui,
                                    &mut chidori_state,
                                    state, &current_theme.theme);
                            }
                            frame.end(ui);
                        }
                        EnclosedState::Close(CloseReason::Failure) => {
                            ui.label("Eval Failure");
                        }
                        EnclosedState::Open => {
                            ui.set_min_width(800.0);
                            ui.label("Executing");
                            egui_execution_state(
                                ui,
                                &mut chidori_state,
                                state, &current_theme.theme);
                        }
                        EnclosedState::SelfContained | EnclosedState::Close(CloseReason::Complete) => {
                            egui_execution_state(ui, &mut chidori_state, state, &current_theme.theme);
                            for (_, value) in state.state.iter() {
                                let image_paths = crate::util::find_matching_strings(&value.output.clone().unwrap(), r"(?i)\.(png|jpe?g)$");
                                for (img_path, _) in image_paths {
                                    // TODO: cache this based on node and the path
                                    let texture = if let Some(cached_texture) = node_image_texture_cache.get(&img_path) {
                                        cached_texture.clone()
                                    } else {
                                        let texture = read_image(ui, &img_path);
                                        node_image_texture_cache.insert(img_path.clone(), texture.clone());
                                        texture
                                    };

                                    // Display the image
                                    ui.add(egui::Image::new(&texture));
                                }
                            }
                        }
                    }
                } else {
                    ui.label("No evaluation recorded");
                    // internal_state.get_execution_state_at_id(*node);
                }
            }

            ui.set_style(original_style);
        }
        frame.end(ui);
    });
}

fn create_egui_texture_image(window: &Window, width: f32, height: f32) -> (f32, u32, u32, Image) {
    let scale_factor = window.scale_factor();
    let scaled_width = (width * scale_factor) as u32;
    let scaled_height = (height * scale_factor) as u32;
    let size = Extent3d {
        width: scaled_width,
        height: scaled_height,
        depth_or_array_layers: 1,
    };
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: None,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
            size
        },
        ..default()
    };
    image.resize(size);
    (scale_factor, scaled_width, scaled_height, image)
}

fn read_image(mut ui: &mut Ui, img_path: &String) -> TextureHandle {
    // Load the image
    let img = image::io::Reader::open(&img_path)
        .expect("Failed to open image")
        .decode()
        .expect("Failed to decode image");

    // Resize the image if necessary
    let resized_img = if img.width() > 512 || img.height() > 512 {
        let ratio = img.width() as f32 / img.height() as f32;
        let (new_width, new_height) = if ratio > 1.0 {
            (512, (512.0 / ratio) as u32)
        } else {
            ((512.0 * ratio) as u32, 512)
        };
        img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Convert the image to egui::ColorImage
    let size = [resized_img.width() as _, resized_img.height() as _];
    let image_buffer = resized_img.to_rgba8();
    let pixels = image_buffer.as_flat_samples();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        size,
        pixels.as_slice(),
    );

    // Create the texture
    let texture = ui.ctx().load_texture(
        img_path,
        color_image,
        egui::TextureOptions::default()
    );
    texture
}

fn generate_tree_layout(
    execution_graph: &&StableGraph<ExecutionNodeId, ()>,
    node_dimensions: &DashMap<ExecutionNodeId, (f32, f32)>
) -> TreeGraph {
    let mut tidy = TidyLayout::new(200., 200., Orientation::Vertical);
    let mut root = crate::tidy_tree::Node::new(0, 600., 80., None);
    let mut tree_graph = crate::tidy_tree::TreeGraph::new(root);

    // Initialize nodes within a TreeGraph using our ExecutionGraph
    let mut topo = petgraph::visit::Topo::new(&execution_graph);
    while let Some(x) = topo.next(&execution_graph) {
        if let Some(node) = &execution_graph.node_weight(x) {
            let dims = node_dimensions.entry(**node).or_insert((800.0, 300.0));
            let mut width = dims.0;
            let mut height = dims.1;
            let tree_node = crate::tidy_tree::Node::new(x.index(), (width) as f64, (height) as f64, Some(Orientation::Vertical));

            // Get parent of this node and attach it if there is one
            let mut parents = &mut execution_graph
                .neighbors_directed(x, petgraph::Direction::Incoming);

            // Only a single parent ever occurs
            if let Some(parent) = &mut parents.next() {
                // TODO: this is the wrong parent identity, this is the parent in the execution graph
                // needs to be in the tree graph
                if let Some(parent_index) = tree_graph.external_id_mapping.get(&parent.index()) {
                    let _ = tree_graph.add_child(parent_index.clone(), tree_node);
                }
            }
        }
    }

    tidy.layout(&mut tree_graph);

    let mut max_y: f32 = 0.0;
    let mut max_x: f32 = 0.0;
    let mut min_x: f32 = 0.0;
    let mut min_y: f32 = 0.0;
    for node in tree_graph.graph.node_weights() {
        max_x = max_x.max(node.x.to_f32().unwrap());
        min_x = min_x.min(node.x.to_f32().unwrap());
        max_y = max_y.max(node.y.to_f32().unwrap());
        min_y = min_y.min(node.y.to_f32().unwrap());
    }

    tree_graph
}


fn update_graph_system_data_structures(
    mut graph_res: ResMut<GraphResource>,
    execution_graph: Res<ChidoriState>,
) {
    // If the execution graph has changed, clear the graph and reconstruct it
    if graph_res.hash_graph != hash_graph(&execution_graph.execution_graph) {
        let (dataset, node_ids) = execution_graph.construct_stablegraph_from_chidori_execution_graph();
        graph_res.node_ids = node_ids;

        let (grouped_dataset, grouped_tree, group_dep_graph) = group_tree(&dataset, &execution_graph.grouped_nodes);

        // TODO: handle support for displaying groups
        // graph_res.execution_graph = grouped_dataset;
        graph_res.execution_graph = dataset;
        graph_res.grouped_tree = grouped_tree;
        graph_res.group_dependency_graph = group_dep_graph;
        graph_res.hash_graph = hash_graph(&execution_graph.execution_graph);
        graph_res.is_layout_dirty = true;
    }
}

fn hash_graph(input: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

fn hash_tuple(input: &ExecutionNodeId) -> usize {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish() as usize
}

#[derive(Component)]
struct ExecutionHeadCursor;

#[derive(Component)]
struct ExecutionSelectionCursor;


fn enforce_tiled_viewports(
    mut graph_resource: ResMut<GraphResource>,
    tree_identities: Res<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    mut main_camera: Query<(&mut Camera, &mut Projection), (With<OnGraphScreen>, With<GraphMainCamera>, Without<GraphMinimapCamera>)>,
    mut mini_camera: Query<(&mut Camera, &mut Projection), (With<OnGraphScreen>, With<GraphMinimapCamera>, Without<GraphMainCamera>)>,
    q_window: Query<&Window, With<PrimaryWindow>>,
) {
    let window = q_window.single();
    let scale_factor = window.scale_factor() as u32;
    let (mut main_camera, _) = main_camera.single_mut();
    let (mut mini_camera, _) = mini_camera.single_mut();
    if let Some(graph_tile) = tree_identities.graph_tile {
        if let Some(tile) = tree.tree.tiles.get(graph_tile) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&graph_tile) {
                        mini_camera.is_active = false;
                        main_camera.is_active = false;
                        graph_resource.is_active = false;
                    } else {
                        mini_camera.is_active = true;
                        main_camera.is_active = true;
                        graph_resource.is_active = true;
                        if &p.nr == &"Graph" {
                            if let Some(r) = p.rect {
                                main_camera.viewport = Some(Viewport {
                                    physical_position: UVec2::new(r.min.x as u32 * scale_factor, r.min.y as u32 * scale_factor),
                                    physical_size: UVec2::new(r.width() as u32 * scale_factor, r.height() as u32 * scale_factor),
                                    ..default()
                                });
                            }
                        }
                    }
                }
                Tile::Container(_) => {}
            }
        }
    }


}

// This struct defines the data that will be passed to your shader
#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
struct RoundedRectMaterial {
    #[uniform(0)]
    pub width: f32,
    #[uniform(1)]
    pub height: f32,

    #[texture(2)]
    #[sampler(3)]
    color_texture: Option<Handle<Image>>,

    #[uniform(4)]
    pub base_color: Vec4,

    alpha_mode: AlphaMode,
}

/// The Material trait is very configurable, but comes with sensible defaults for all methods.
/// You only need to implement functions for features that need non-default behavior. See the Material api docs for details!
impl Material for RoundedRectMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://chidori_debugger/../assets/shaders/rounded_rect.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }
}


fn update_node_materials(
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut materials_custom: ResMut<Assets<RoundedRectMaterial>>,
) {
    for (_, t, _, mh) in node_query.iter_mut() {
        if let Some(mat) = materials_custom.get_mut(mh) {
            mat.width = t.scale.x;
            mat.height = t.scale.y;
        }
    }
}

fn update_cursor_materials(
    mut execution_head_cursor: Query<
        (Entity, &mut Transform, &Handle<RoundedRectMaterial>),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>, Without<GraphMainCamera>),
    >,
    mut execution_selection_cursor: Query<
        (Entity, &mut Transform, &Handle<RoundedRectMaterial>),
        (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>, Without<GraphMainCamera>),
    >,
    mut materials: ResMut<Assets<RoundedRectMaterial>>
) {

    let (_, t, mh) = execution_head_cursor.single_mut();
    if let Some(mat) = materials.get_mut(mh) {
        mat.width = t.scale.x;
        mat.height = t.scale.y;
    }
    let (_, t, mh) = execution_selection_cursor.single_mut();
    if let Some(mat) = materials.get_mut(mh) {
        mat.width = t.scale.x;
        mat.height = t.scale.y;
    }
}

fn graph_setup(
    windows: Query<&Window>,
    mut commands: Commands,
    execution_graph: Res<ChidoriState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials_standard: ResMut<Assets<StandardMaterial>>,
    mut materials_custom: ResMut<Assets<RoundedRectMaterial>>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();

    // let (config, _) = config_store.config_mut::<DefaultGizmoConfigGroup>();
    // config.line_width = 1.0;
    // config.render_layers = RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW);

    let cursor_selection_material = materials_custom.add(RoundedRectMaterial {
        width: 1.0,
        height: 1.0,
        color_texture: None,
        base_color: Vec4::new(0.565, 1.00, 0.882, 0.00),
        alpha_mode: AlphaMode::Blend,
    });

    let cursor_head_material = materials_custom.add(RoundedRectMaterial {
        width: 1.0,
        height: 1.0,
        color_texture: None,
        base_color: Vec4::new(0.882, 0.00392, 0.357, 0.0),
        alpha_mode: AlphaMode::Blend,
    });

    // Main camera
    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 2,
                clear_color: ClearColorConfig::Custom(Color::rgba(0.035, 0.035, 0.043, 1.0)),
                // viewport: Some(Viewport {
                //     physical_position: UVec2::new((300.0 * scale_factor) as u32, 0),
                //     physical_size: UVec2::new(window.physical_width() - 300 * (scale_factor as u32), window.physical_height()),
                //     ..default()
                // }),
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

    // Minimap viewport indicator
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))).into(),
            material: materials_standard.add(Color::hsla(3.0, 1.0, 1.0, 0.8)),
            transform: Transform::from_xyz(0.0, -50.0, -30.0),
            ..default()
        },
        RenderLayers::layer(RENDER_LAYER_GRAPH_MINIMAP),
        GraphMinimapViewportIndicator,
        Collider::cuboid(0.5, 0.5),
        Sensor,
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

    // let shape = shapes::RegularPolygon {
    //     sides: 6,
    //     feature: shapes::RegularPolygonFeature::Radius(200.0),
    //     ..shapes::RegularPolygon::default()
    // };
    //
    // commands.spawn((
    //     ShapeBundle {
    //         path: GeometryBuilder::build_as(&shape),
    //         ..default()
    //     },
    //     crate::bevy_prototype_lyon::prelude::Fill::color(bevy::prelude::Color::CYAN),
    //     crate::bevy_prototype_lyon::prelude::Stroke::new(bevy::prelude::Color::BLACK, 10.0),
    // ));

    let mut dataset = StableGraph::new();
    let mut node_ids = HashMap::new();
    commands.spawn((CursorWorldCoords(vec2(0.0, 0.0)), OnGraphScreen));
    commands.insert_resource(GraphResource {
        execution_graph: dataset,
        group_dependency_graph: Default::default(),
        hash_graph: hash_graph(&execution_graph.execution_graph),
        node_ids,
        node_dimensions: Default::default(),
        grouped_tree: Default::default(),
        is_active: false,
        is_layout_dirty: true,
        layout_graph: None,
    });
}

fn ui_window(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut chidori_state: ResMut<ChidoriState>,
    current_theme: Res<CurrentTheme>,
    mut tree: ResMut<EguiTree>,
) {
    let window = q_window.single();
    let mut hide_all = false;

    let mut container_frame = Frame::default()
        .fill(current_theme.theme.accent)
        .outer_margin(Margin {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        })
        .inner_margin(16.0);
    if let Some(graph_title) = tree_identities.graph_tile {
        if let Some(tile) = tree.tree.tiles.get(graph_title) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&graph_title) {
                        hide_all = true;
                    } else {
                        if let Some(r) = p.rect {
                            container_frame = container_frame.outer_margin(Margin {
                                left: r.min.x,
                                right: (window.width() - 300.0),
                                top: r.min.y,
                                bottom: window.height() - r.max.y,
                            });
                        }
                    }
                }
                Tile::Container(_) => {}
            }
        }
    }

    if hide_all || chidori_state.display_example_modal {
        return;
    }

    egui::CentralPanel::default().frame(container_frame).show(contexts.ctx_mut(), |ui| {
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.vertical(|ui| {
                ui.label("Sidebar");
                ui.button("Collapse Alternate Branches");
            });
        });

    });
    // egui::SidePanel::left("Explorer")
    //     .frame(container_frame)
    //     .min_width(300.0)
    //     .max_width(300.0)
    //     .resizable(false)
    //     .show_separator_line(false)
    //     .show(contexts.ctx_mut(), |ui| {
    //     });
}

#[derive(Component)]
struct OnGraphScreen;

pub fn graph_plugin(app: &mut App) {
    embedded_asset!(app, "../assets/shaders/rounded_rect.wgsl");
    app.init_resource::<NodeIdToEntity>()
        .init_resource::<EdgePairIdToEntity>()
        .init_resource::<SelectedEntity>()
        .init_resource::<InteractionLock>()
        .add_plugins(MaterialPlugin::<RoundedRectMaterial>::default())
        .add_systems(OnEnter(crate::GameState::Graph), graph_setup)
        .add_systems(
            OnExit(crate::GameState::Graph),
            despawn_screen::<OnGraphScreen>,
        )
        .add_systems(
            Update,
            (
                keyboard_navigate_graph,
                compute_egui_transform_matrix,
                mouse_pan,
                set_camera_viewports,
                update_minimap_camera_configuration,
                update_trace_space_to_camera_configuration,
                camera_follow_selection_head,
                node_cursor_handling,
                touchpad_gestures,
                update_graph_system_renderer.after(mouse_scroll_events),
                update_graph_system_data_structures,
                my_cursor_system,
                mouse_scroll_events,
                mouse_over_system,
                enforce_tiled_viewports,
                update_cursor_materials,
                update_node_materials,
                ui_window,
                render_graph_grouping
            )
                .run_if(in_state(GameState::Graph)),
        );
}
