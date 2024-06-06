use crate::chidori::{ChidoriExecutionGraph, EguiTree, EguiTreeIdentities};
use crate::tidy_tree::{Layout, TidyLayout};
use crate::util::{despawn_screen, egui_render_cell_read};
use crate::{GameState, RENDER_LAYER_GRAPH_MINIMAP, RENDER_LAYER_GRAPH_VIEW, RENDER_LAYER_TRACE_MINIMAP, RENDER_LAYER_TRACE_VIEW, util};
use bevy::app::{App, Update};
use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::input::touchpad::TouchpadMagnify;
use bevy::math::{vec2, vec3, Vec3};
use bevy::prelude::*;
use bevy::prelude::{
    Assets, Circle, Color, Commands, Component, default, in_state,
    IntoSystemConfigs, Mesh, OnEnter, OnExit, ResMut, Transform,
};
use bevy::render::render_resource::{AsBindGroup, ShaderRef};
use bevy::render::view::{NoFrustumCulling, RenderLayers};
use bevy::tasks::futures_lite::StreamExt;
use bevy::utils::petgraph::stable_graph::GraphIndex;
use bevy::window::{PrimaryWindow, WindowResized};
use bevy_egui::egui::{Color32, Order, Pos2, RichText, Ui};
use bevy_egui::{egui, EguiContexts};
use bevy_rapier2d::geometry::Collider;
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use bevy_rapier2d::prelude::*;
use chidori_core::execution::execution::execution_graph::ExecutionNodeId;
use chidori_core::execution::execution::ExecutionState;
use fdg::petgraph::graph::NodeIndex;
use fdg::ForceGraph;
use num::ToPrimitive;
use petgraph::data::DataMap;
use petgraph::prelude::StableGraph;
use petgraph::visit::Walker;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ptr::NonNull;
use bevy::render::camera::{ScalingMode, Viewport};
use egui_tiles::Tile;
use chidori_core::execution::execution::execution_state::ExecutionStateEvaluation;

#[derive(Resource, Default)]
struct SelectedEntity {
    id: Option<Entity>,
}

#[derive(Resource)]
struct GraphResource {
    graph: ForceGraph<f32, 2, ExecutionNodeId, ()>,
    hash_graph: u64,
    node_ids: HashMap<ExecutionNodeId, NodeIndex>,
    is_active: bool
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
struct GraphMinimapCamera;

enum CameraStateValue {
    Locked,
    Free
}

#[derive(Component)]
struct CameraState {
    state: CameraStateValue
}


fn update_minimap_camera_configuration(
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnGraphScreen>, With<GraphMinimapCamera>)>,
) {
    let (projection, mut camera_transform) = camera.single_mut();
    let (mut scale) = match projection.into_inner() {
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
    windows: Query<&Window>,
    mut main_camera: Query<(&mut Projection, &mut Transform), (With<GraphMainCamera>, Without<GraphMinimapCamera>)>,
    mut minimap_camera: Query<(&mut Projection, &mut Transform), (With<GraphMinimapCamera>, Without<GraphMainCamera>)>,
    mut minimap_viewport_indicator: Query<(&mut Transform), (With<GraphMinimapViewportIndicator>, Without<GraphMainCamera>, Without<GraphMinimapCamera>)>,
) {

    let window = windows.single();
    let scale_factor = window.scale_factor();
    let (main_projection, mut main_camera_transform) = main_camera.single_mut();
    let (mini_projection, mut mini_camera_transform) = minimap_camera.single_mut();

    let main_projection = match main_projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { o }
    };
    let mini_projection = match mini_projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { o }
    };

    let camera_position = mini_camera_transform.translation;
    let main_viewport_width = main_projection.area.width();
    let main_viewport_height = main_projection.area.height();
    let viewport_width = mini_projection.area.width();

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
    mut main_camera: Query<(&mut Camera, &mut Projection), (With<GraphMainCamera>, Without<GraphMinimapCamera>)>,
    mut minimap_camera: Query<&mut Camera, (With<GraphMinimapCamera>, Without<GraphMainCamera>)>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();
    // let minimap_offset = crate::traces::MINIMAP_OFFSET * scale_factor as u32;
    // let minimap_height = (crate::traces::MINIMAP_HEIGHT as f32 * scale_factor) as u32;
    // let minimap_height_and_offset = crate::traces::MINIMAP_HEIGHT_AND_OFFSET * scale_factor as u32;
    let (mut main_camera , mut projection) = main_camera.single_mut();
    let mut minimap_camera = minimap_camera.single_mut();

    // We need to dynamically resize the camera's viewports whenever the window size changes
    // so then each camera always takes up half the screen.
    // A resize_event is sent when the window is first created, allowing us to reuse this system for initial setup.
    for resize_event in resize_events.read() {
        // main_camera.viewport = Some(Viewport {
        //     physical_position: UVec2::new(0, minimap_height_and_offset),
        //     physical_size: UVec2::new(
        //         window.resolution.physical_width(),
        //         window.resolution.physical_height() - minimap_height_and_offset,
        //     ),
        //     ..default()
        // });
        // minimap_camera.viewport = Some(Viewport {
        //     physical_position: UVec2::new(0, minimap_offset),
        //     physical_size: UVec2::new(
        //         window.resolution.physical_width(),
        //         minimap_height,
        //     ),
        //     ..default()
        // });
    }
}

fn mouse_pan(
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen>, Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>)>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion_evr: EventReader<MouseMotion>,
) {
    let (projection, mut camera_transform, mut camera_state) = q_camera.single_mut();
    if buttons.pressed(MouseButton::Left) {
        for ev in motion_evr.read() {
            camera_transform.translation.x -= ev.delta.x;
            camera_transform.translation.y += ev.delta.y;
        }
        camera_state.state = CameraStateValue::Free;
    }
}


fn mouse_scroll_events(
    graph_resource: Res<GraphResource>,
    mut scroll_evr: EventReader<MouseWheel>,
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen> , Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>)>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    if !graph_resource.is_active {
        return;
    }
    let (projection, mut camera_transform, mut camera_state) = q_camera.single_mut();

    if keyboard_input.just_pressed(KeyCode::Enter) {
        camera_state.state = CameraStateValue::Locked;
    }

    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };

    for ev in scroll_evr.read() {
        if keyboard_input.pressed(KeyCode::SuperLeft) {
            // projection.scale = (projection.scale + ev.y).clamp(0.5, 1000.0);
            // let mut style: egui::Style = (*ctx.style()).clone();
            // style.text_styles.get_mut(&egui::TextStyle::Body).unwrap().size -= ev.y;
            // ctx.set_style(style);
        } else {
            camera_state.state = CameraStateValue::Free;
            camera_transform.translation.x -= ev.x * projection.scale;
            camera_transform.translation.y += ev.y * projection.scale;
        }
    }

}

fn touchpad_gestures(
    mut q_camera: Query<(&mut Projection, &GlobalTransform), With<OnGraphScreen>>,
    mut evr_touchpad_magnify: EventReader<TouchpadMagnify>,
) {
    let (projection, camera_transform) = q_camera.single_mut();
    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };
    for ev_magnify in evr_touchpad_magnify.read() {
        projection.scale += ev_magnify.0;
    }
}


fn my_cursor_system(
    mut q_mycoords: Query<&mut CursorWorldCoords, With<OnGraphScreen>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Camera, &GlobalTransform), (With<OnGraphScreen>, Without<GraphMinimapCamera>)>,
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

fn egui_execution_state(ui: &mut Ui, execution_state: &ExecutionState) {
    ui.vertical(|ui| {
        ui.label("Evaluated:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(format!("Operation Id: {:?}", execution_state.evaluating_id));
                ui.label(format!("Cell Name: {:?}", execution_state.evaluating_name.as_ref().unwrap_or(&String::from("Unnamed"))));
                if let Some(evaluating_fn) = &execution_state.evaluating_fn {
                    ui.label(format!("Function Invoked: {:?}", evaluating_fn));
                }
            })
        });

        if let Some(cell) = &execution_state.operation_mutation {
            egui_render_cell_read(ui, cell);
        }
        for (key, value) in execution_state.state.iter() {
            ui.horizontal(|ui| {
                ui.label(format!("{:?}", key));
                ui.separator();
                util::egui_rkyv(ui, &value.output, false);
            });
        }
    });
}

fn camera_follow_selection_head(
    mut q_camera: Query<(&Camera, &mut Transform, &CameraState), (With<OnGraphScreen>,  With<GraphMainCamera>, Without<ExecutionSelectionCursor>, Without<GraphMinimapCamera>)>,
    execution_graph: ResMut<crate::chidori::ChidoriExecutionGraph>,
    // mut execution_selection_query: Query<
    //     (Entity, &mut Transform),
    //     (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>),
    // >,
    mut execution_head_cursor: Query<
        (Entity, &mut Transform),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>, Without<GraphMainCamera>),
    >,
) {
    let (camera, mut camera_transform, camera_state) = q_camera.single_mut();
    let (_, mut t) = execution_head_cursor.single_mut();
    if matches!(camera_state.state, CameraStateValue::Locked) {
        camera_transform.translation.x = t.translation.x;
        camera_transform.translation.y = t.translation.y;
    }
}

fn mouse_over_system(
    mut graph_resource: ResMut<GraphResource>,
    mut commands: Commands,
    buttons: Res<ButtonInput<MouseButton>>,
    q_mycoords: Query<&CursorWorldCoords, With<OnGraphScreen>>,
    mut selected_entity: ResMut<SelectedEntity>,
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut gizmos: Gizmos,
    mut contexts: EguiContexts,
    rapier_context: Res<RapierContext>,
    q_camera: Query<(&Camera, &GlobalTransform), (With<OnGraphScreen>, Without<GraphMinimapCamera>)>,
    internal_state: ResMut<crate::chidori::InternalState>,
    exec_id_to_state: ResMut<crate::chidori::ChidoriExecutionIdsToStates>,
) {
    if !graph_resource.is_active {
        return;
    }
    let ctx = contexts.ctx_mut();
    // https://docs.rs/bevy/latest/bevy/prelude/enum.CursorIcon.html

    let (camera, camera_transform) = q_camera.single();
    let cursor = q_mycoords.single();

    for (_, _, mut gidx) in node_query.iter_mut() {
        gidx.is_hovered = false;
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
        if let Ok((_, t, mut gidx)) = node_query.get_mut(entity) {
            gidx.is_hovered = true;

            if buttons.just_pressed(MouseButton::Left) {
                gidx.is_selected = true;
                selected_entity.id = Some(entity);
            }
        }

        false
    });

    // Deselect others
    for (entity, _, mut gidx) in node_query.iter_mut() {
        if Some(entity) != selected_entity.id {
            gidx.is_selected = false;
        }
    }
}

fn node_cursor_handling(
    mut commands: Commands,
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
    execution_graph: ResMut<crate::chidori::ChidoriExecutionGraph>,
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
                return;
            } else {
                if Some(entity) == selected_entity.id {
                    let (_, mut t) = execution_selection_query.single_mut();
                    t.translation.x = node_transform.translation.x;
                    t.translation.y = node_transform.translation.y;
                    t.scale = node_transform.scale + 10.0;
                    t.translation.z = -2.0;
                    return;
                }
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


fn update_alternate_graph_system(
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut commands: Commands,
    mut contexts: EguiContexts,
    mut graph_resource: ResMut<GraphResource>,
    mut edge_pair_id_to_entity: ResMut<EdgePairIdToEntity>,
    mut node_id_to_entity: ResMut<NodeIdToEntity>,
    mut node_query: Query<
        (Entity, &mut Transform, &GraphIdx),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut edge_query: Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut materials_custom: ResMut<Assets<CustomMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    q_camera: Query<(&Camera, &GlobalTransform), (With<OnGraphScreen>, Without<GraphMinimapCamera>)>,
    mut node_index_to_entity: Local<HashMap<usize, Entity>>,
    exec_id_to_state: ResMut<crate::chidori::ChidoriExecutionIdsToStates>,
    internal_state: ResMut<crate::chidori::InternalState>,
) {
    // TODO: something in this logic is affecting the trace rendering
    if !graph_resource.is_active {
        return;
    }
    let window = q_window.single();
    let scale_factor = window.scale_factor() as f32;
    let (camera, camera_transform) = q_camera.single();
    let viewport_pos = if let Some(viewport) = camera.viewport.as_ref() {
        viewport.physical_position
    } else {
        UVec2::new(0, 0)
    };
    let viewport_size = if let Some(viewport) = camera.viewport.as_ref() {
        viewport.physical_size
    } else {
        UVec2::new(0, 0)
    };
    let ctx = contexts.ctx_mut();
    let mut topo = petgraph::visit::Topo::new(&graph_resource.graph);
    let mut node_mapping: HashMap<NodeIndex, NonNull<crate::tidy_tree::Node>> = HashMap::new();
    let mut tidy = TidyLayout::new(200., 200.);
    let mut root = crate::tidy_tree::Node::new(0, 10., 10.);
    while let Some(x) = topo.next(&graph_resource.graph) {
        if let Some(node) = &graph_resource.graph.node_weight(x) {

            // Mock the layout of these elements to determine the resulting sizing
            let mut width = 100.0;
            let mut height = 100.0;

            // TODO: we offset this slightly because it appears to be affecting our navigation UI
            //       when it is positioned at 0,0 regardless of its interactable configuration
            egui::Area::new(format!("{:?}", node.0).into())
                .fixed_pos(Pos2::new(0.0, 100.0))
                .default_pos(Pos2::new(0.0, 0.0))
                .order(Order::Background)
                .movable(false)
                .interactable(false)
                .enabled(false)
                .constrain(false)
                .show(ctx, |ui| {
                    // Save the current clip rect
                    let original_clip_rect = ui.clip_rect();

                    // Create a large clip rect to force rendering, but move it entirely offscreen
                    let large_clip_rect = egui::Rect::from_min_max(Pos2::new(8000.0, 8000.0), Pos2::new(20000.0, 20000.0));
                    ui.set_clip_rect(large_clip_rect);

                    let mut frame = egui::Frame::default().outer_margin(16.0).rounding(6.0).begin(ui);
                    {
                        render_node(&node.0, &exec_id_to_state.inner, &internal_state, true, ui);
                    }
                    frame.end(ui);

                    // Restore the original clip rect
                    ui.set_clip_rect(original_clip_rect);

                    let rect = ui.min_rect();
                    width = rect.width();
                    height = rect.height();
            });


            let tree_node = crate::tidy_tree::Node::new(x.index(), (width) as f64, (height) as f64);
            let mut parents = &mut graph_resource
                .graph
                .neighbors_directed(x, petgraph::Direction::Incoming);
            // Only a single parent ever occurs
            if let Some(parent) = &mut parents.next() {
                if let Some(parent) = node_mapping.get_mut(parent) {
                    unsafe {
                        let parent = parent.as_mut();
                        let node = parent.append_child(tree_node);
                        node_mapping.insert(x, node);
                    }
                }
            } else {
                let node = root.append_child(tree_node);
                node_mapping.insert(x, node);
            }
        }
    }

    tidy.layout(&mut root);

    let mut topo = petgraph::visit::Topo::new(&graph_resource.graph);
    while let Some(idx) = topo.next(&graph_resource.graph) {
        if let Some(node) = &graph_resource.graph.node_weight(idx) {
            let mut parents = &mut graph_resource
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming);
            let parent_pos = parents
                .next()
                .and_then(|parent| node_id_to_entity.mapping.get(&parent))
                .and_then(|entity| {
                    if let Ok((_, mut transform, _)) = node_query.get_mut(*entity) {
                        Some(transform.translation.truncate())
                    } else {
                        None
                    }
                }).unwrap_or(vec2(0.0, 0.0));

            if let Some(n) = node_mapping.get(&idx) {
                unsafe {
                    let n = n.as_ref();
                    let entity = node_id_to_entity.mapping.entry(idx).or_insert_with(|| {
                        let node_material = materials_custom.add(CustomMaterial {
                            width: 1.0,
                            height: 1.0,
                            color: Vec4::new(1.0, 1.0, 1.0, 0.9),
                            alpha_mode: AlphaMode::Blend,
                        });

                        let entity = commands.spawn((
                            MaterialMeshBundle {
                                mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
                                material: node_material,
                                transform: Transform::from_xyz(parent_pos.x, parent_pos.y, -1.0),
                                ..Default::default()
                            },
                            GraphIdx {
                                loading: false,
                                execution_id: node.0,
                                id: idx.index(),
                                is_hovered: false,
                                is_selected: false,
                            },
                            Sensor,
                            Collider::cuboid(0.5, 0.5),
                            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
                            OnGraphScreen
                        ));
                        node_index_to_entity.insert(n.id, entity.id());
                        entity.id()
                    });

                    if let Ok((entity, mut transform, gidx)) = node_query.get_mut(*entity) {
                        // TODO: working theory is that positioning the elements is affecting the camera which is affecting the trace camera
                        //       the trace camera is still be affected by other camera values
                        transform.translation = Vec3::new(n.x.to_f32().unwrap(), -n.y.to_f32().unwrap(), -1.0);
                        transform.scale =  vec3(n.width.to_f32().unwrap() + 20.0, n.height.to_f32().unwrap() + 20.0, 1.0);

                        // Draw text on top of these elements
                        if let Some(mut pos) = camera.world_to_viewport(camera_transform, transform.translation) {
                            let camera_viewport_pos = if let Some(viewport) = camera.viewport.as_ref() {
                                viewport.physical_position.clone()
                            } else {
                                UVec2::new(0,0)
                            };
                            pos += vec2(camera_viewport_pos.x as f32 / scale_factor, camera_viewport_pos.y as f32 / scale_factor);
                            egui::Area::new(format!("{:?}", entity).into())
                                .fixed_pos(Pos2::new(pos.x, pos.y)).show(ctx, |ui| {

                                let original_clip_rect = ui.clip_rect();

                                let viewport_max = viewport_pos + viewport_size;
                                // Create a large clip rect to force rendering, but move it entirely offscreen
                                let large_clip_rect = egui::Rect::from_min_max(Pos2::new(
                                    viewport_pos.x as f32 / scale_factor, viewport_pos.y as f32 / scale_factor
                                ), Pos2::new(viewport_max.x as f32, viewport_max.y as f32));
                                ui.set_clip_rect(large_clip_rect);

                                let mut frame = egui::Frame::default().outer_margin(16.0).rounding(6.0).begin(ui);
                                {
                                    render_node(&node.0, &exec_id_to_state.inner, &internal_state, gidx.is_selected, ui);
                                }
                                frame.end(ui);

                                // Restore the original clip rect
                                ui.set_clip_rect(original_clip_rect);
                            });
                        }
                        transform.translation.x += n.width.to_f32().unwrap() / 2.0;
                        transform.translation.y -= n.height.to_f32().unwrap() / 2.0;
                    }

                    n.children.iter().for_each(|child| {
                        let parent_pos = if let Some(entity ) = node_index_to_entity.get(&n.id) {
                            if let Ok((entity, mut transform, gidx)) = node_query.get(*entity) {
                                transform.translation.truncate()
                            } else {
                                return;
                            }
                        } else {
                            return;
                        };
                        let child_pos = if let Some(entity ) = node_index_to_entity.get(&child.id) {
                            if let Ok((entity, mut transform, gidx)) = node_query.get(*entity) {
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

                        let entity = edge_pair_id_to_entity.mapping.entry((n.id, child.id)).or_insert_with(|| {
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
                                    source: n.id,
                                    target: child.id,
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
    }
}

fn render_node(
    node: &ExecutionNodeId,
    exec_id_to_state: &HashMap<ExecutionNodeId, ExecutionState>,
    internal_state: &crate::chidori::InternalState,
    enable_scrolling: bool,
    ui: &mut Ui
) {
    let original_style = (*ui.ctx().style()).clone();

    let mut style = original_style.clone();
    style.visuals.override_text_color = Some(Color32::BLACK);
    ui.set_style(style);

    egui::ScrollArea::new([false, true]) // Horizontal: false, Vertical: true
        .max_width(700.0)
        .max_height(400.0)
        .show(ui, |ui| {
            if node == &(0, 0) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Initialization...").color(Color32::BLACK));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.button(RichText::new("Revert to this State").color(Color32::from_hex("#dddddd").unwrap())).clicked() {
                            let _ = internal_state.set_execution_id(*node);
                        }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.button(RichText::new("Revert to this State").color(Color32::from_hex("#dddddd").unwrap())).clicked() {
                            let _ = internal_state.set_execution_id(*node);
                        }
                    });
                });


                if let Some(state) = internal_state.chidori.lock().unwrap().get_shared_state().execution_id_to_evaluation.get(&node) {
                    if let ExecutionStateEvaluation::Complete(state) = state {
                        egui_execution_state(ui, state);
                    }
                } else {
                    // internal_state.get_execution_state_at_id(*node);
                }
            }
        });

    ui.set_style(original_style);
    return;
}

fn update_graph_system(
    mut graph_res: ResMut<GraphResource>,
    mut execution_graph: ResMut<ChidoriExecutionGraph>,
) {
    // If the execution graph has changed, clear the graph and reconstruct it
    if graph_res.hash_graph != hash_graph(&execution_graph.inner) {
        let mut dataset = StableGraph::new();
        let mut node_ids = HashMap::new();
        for (a, b) in &execution_graph.inner {
            let node_index_a = *node_ids
                .entry(a.clone())
                .or_insert_with(|| dataset.add_node(a.clone()));
            let node_index_b = *node_ids
                .entry(b.clone())
                .or_insert_with(|| dataset.add_node(b.clone()));
            dataset.add_edge(node_index_a, node_index_b, ());
        }
        let mut graph: ForceGraph<f32, 2, ExecutionNodeId, ()> =
            fdg::init_force_graph_uniform(dataset, 30.0);
        graph_res.node_ids = node_ids;
        graph_res.graph = graph;
        graph_res.hash_graph = hash_graph(&execution_graph.inner);
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


// TODO: capture the clip area we want to enforce for the egui elements
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
    let (mut main_camera, mut projection) = main_camera.single_mut();
    let (mut mini_camera, mut projection) = mini_camera.single_mut();
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
struct CustomMaterial {
    #[uniform(0)]
    pub width: f32,
    #[uniform(1)]
    pub height: f32,
    #[uniform(2)]
    pub color: Vec4,
    alpha_mode: AlphaMode,
}

/// The Material trait is very configurable, but comes with sensible defaults for all methods.
/// You only need to implement functions for features that need non-default behavior. See the Material api docs for details!
impl Material for CustomMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/rounded_rect.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }
}


fn update_node_materials(
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx, &Handle<CustomMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut materials_custom: ResMut<Assets<CustomMaterial>>,
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
        (Entity, &mut Transform, &Handle<CustomMaterial>),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>, Without<GraphMainCamera>),
    >,
    mut execution_selection_cursor: Query<
        (Entity, &mut Transform, &Handle<CustomMaterial>),
        (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>, Without<GraphMainCamera>),
    >,
    mut materials: ResMut<Assets<CustomMaterial>>
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
    mut config_store: ResMut<GizmoConfigStore>,
    mut commands: Commands,
    mut execution_graph: ResMut<ChidoriExecutionGraph>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials_standard: ResMut<Assets<StandardMaterial>>,
    mut materials_custom: ResMut<Assets<CustomMaterial>>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();

    // let (config, _) = config_store.config_mut::<DefaultGizmoConfigGroup>();
    // config.line_width = 1.0;
    // config.render_layers = RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW);

    let cursor_selection_material = materials_custom.add(CustomMaterial {
        width: 1.0,
        height: 1.0,
        color: Vec4::new(0.565, 1.00, 0.882, 1.00),
        alpha_mode: AlphaMode::Blend,
    });

    let cursor_head_material = materials_custom.add(CustomMaterial {
        width: 1.0,
        height: 1.0,
        color: Vec4::new(0.882, 0.00392, 0.357, 1.0),
        alpha_mode: AlphaMode::Blend,
    });

    commands.spawn((
        Camera3dBundle {
            transform: Transform::from_xyz(0.0, 0.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
            camera: Camera {
                order: 1,
                clear_color: ClearColorConfig::Custom(Color::rgba(0.1, 0.1, 0.1, 1.0)),
                ..default()
            },
            projection: OrthographicProjection {
                scale: 1.0,
                ..default()
            }.into(),
            ..default()
        },
        OnGraphScreen,
        GraphMainCamera,
        CameraState { state: CameraStateValue::Locked },
        RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
    ));

    commands.spawn((
        Camera3dBundle {
            transform: Transform::from_xyz(0.0, 0.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
            camera: Camera {
                order: 2,
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
        CameraState { state: CameraStateValue::Locked },
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

    let entity_execution_head = commands.spawn((
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

    let mut dataset = StableGraph::new();
    let mut node_ids = HashMap::new();
    // for (a, b) in &execution_graph.inner {
    //     let node_index_a = *node_ids
    //         .entry(a.clone())
    //         .or_insert_with(|| dataset.add_node(a.clone()));
    //     let node_index_b = *node_ids
    //         .entry(b.clone())
    //         .or_insert_with(|| dataset.add_node(b.clone()));
    //     dataset.add_edge(node_index_a, node_index_b, ());
    // }
    let mut graph: ForceGraph<f32, 2, ExecutionNodeId, ()> =
        fdg::init_force_graph_uniform(dataset, 30.0);
    commands.spawn((CursorWorldCoords(vec2(0.0, 0.0)), OnGraphScreen));
    commands.insert_resource(GraphResource {
        graph,
        hash_graph: hash_graph(&execution_graph.inner),
        node_ids,
        is_active: false
    });
}

#[derive(Component)]
struct OnGraphScreen;

pub fn graph_plugin(app: &mut App) {
    app.init_resource::<NodeIdToEntity>()
        .init_resource::<EdgePairIdToEntity>()
        .init_resource::<SelectedEntity>()
        .add_plugins(MaterialPlugin::<CustomMaterial>::default())
        .add_systems(OnEnter(crate::GameState::Graph), graph_setup)
        .add_systems(
            OnExit(crate::GameState::Graph),
            despawn_screen::<OnGraphScreen>,
        )
        .add_systems(
            Update,
            (
                mouse_pan,
                set_camera_viewports,
                update_minimap_camera_configuration,
                update_trace_space_to_camera_configuration,
                camera_follow_selection_head,
                node_cursor_handling,
                // touchpad_gestures,
                update_alternate_graph_system,
                update_graph_system,
                my_cursor_system,
                mouse_scroll_events,
                mouse_over_system,
                enforce_tiled_viewports,
                update_cursor_materials,
                update_node_materials
            )
                .run_if(in_state(GameState::Graph)),
        );
}
