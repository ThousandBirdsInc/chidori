use crate::chidori::{ChidoriExecutionGraph, EguiTree};
use crate::tidy_tree::{Layout, TidyLayout, TidyTree};
use crate::util::{
    change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text,
};
use crate::GameState;
use bevy::app::{App, Update};
use bevy::asset::Asset;
use bevy::input::mouse::MouseWheel;
use bevy::input::touchpad::TouchpadMagnify;
use bevy::math::{vec2, vec3, Vec3};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::prelude::{
    default, in_state, Assets, Camera2dBundle, Circle, Color, ColorMaterial, Commands, Component,
    IntoSystemConfigs, MaterialMeshBundle, Mesh, OnEnter, OnExit, ResMut, Transform, TypePath,
};
use bevy::render::mesh::{MeshVertexBufferLayout, PrimitiveTopology, VertexAttributeValues};
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{
    AsBindGroup, PolygonMode, RenderPipelineDescriptor, ShaderRef, SpecializedMeshPipelineError,
};
use bevy::render::view::{NoFrustumCulling, RenderLayers};
use bevy::sprite::{MaterialMesh2dBundle, Mesh2dHandle};
use bevy::tasks::futures_lite::StreamExt;
use bevy::utils::petgraph::stable_graph::GraphIndex;
use bevy::window::{PrimaryWindow, WindowResized};
use bevy_cosmic_edit::{CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem};
use bevy_egui::egui::{Pos2, Ui};
use bevy_egui::{egui, EguiContext, EguiContexts};
use bevy_rapier2d::geometry::Collider;
use bevy_rapier2d::parry::transformation::utils::transform;
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use bevy_rapier2d::prelude::*;
use chidori_core::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId};
use chidori_core::execution::execution::ExecutionState;
use chidori_core::execution::primitives::serialized_value::RkyvSerializedValue;
use chidori_core::sdk::entry::Chidori;
use egui_extras::{Column, TableBuilder};
use fdg::nalgebra::{Const, OVector, Point};
use fdg::petgraph::graph::NodeIndex;
use fdg::{fruchterman_reingold::FruchtermanReingold, simple::Center, Force, ForceGraph};
use num::ToPrimitive;
use petgraph::adj::DefaultIx;
use petgraph::data::DataMap;
use petgraph::graph::DiGraph;
use petgraph::prelude::{DiGraphMap, StableGraph};
use petgraph::visit::Walker;
use petgraph::Directed;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;
use bevy::core_pipeline::fxaa::FxaaPlugin;
use bevy::render::camera::Viewport;
use egui_tiles::Tile;

#[derive(Resource, Default)]
struct SelectedEntity {
    id: Option<Entity>,
}

#[derive(Resource)]
struct GraphResource {
    graph: ForceGraph<f32, 2, ExecutionNodeId, ()>,
    hash_graph: u64,
    node_ids: HashMap<ExecutionNodeId, NodeIndex>,
}

#[derive(Component)]
struct GraphIdx {
    loading: bool,
    execution_id: ExecutionNodeId,
    id: usize,
    is_hovered: bool,
}

#[derive(Component)]
struct GraphIdxPair {
    source: usize,
    target: usize,
}

#[derive(Component, Default)]
struct CursorWorldCoords(Vec2);

fn mouse_scroll_events(
    mut scroll_evr: EventReader<MouseWheel>,
    mut q_camera: Query<(&mut Projection, &mut Transform), With<OnGraphScreen>>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    let (projection, mut camera_transform) = q_camera.single_mut();
    let mut projection = match projection.into_inner() {
        Projection::Perspective(_) => {
            unreachable!("This should be orthographic")
        }
        Projection::Orthographic(ref mut o) => o,
    };
    for ev in scroll_evr.read() {
        if keyboard_input.pressed(KeyCode::SuperLeft) {
            projection.scale = (projection.scale + ev.y).clamp(1.0, 1000.0);
        } else {
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
    q_camera: Query<(&Camera, &GlobalTransform), With<OnGraphScreen>>,
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


fn egui_rkyv(ui: &mut Ui, value: &RkyvSerializedValue) {
    match value {
        RkyvSerializedValue::StreamPointer(_) => {}
        RkyvSerializedValue::FunctionPointer(_, _) => {}
        RkyvSerializedValue::Cell(_) => {}
        RkyvSerializedValue::Set(_) => {}
        RkyvSerializedValue::Float(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Number(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::String(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Boolean(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Null => {}
        RkyvSerializedValue::Array(a) => {
            ui.vertical(|ui| {
                ui.label("Array");
                ui.separator();
                for (key, value) in a.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{:?}", key));
                        ui.separator();
                        egui_rkyv(ui, value);
                    });
                }
            });
        }
        RkyvSerializedValue::Object(o) => {
            ui.vertical(|ui| {
                ui.label("Object");
                ui.separator();
                for (key, value) in o.iter() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{:?}", key));
                        ui.separator();
                        egui_rkyv(ui, value);
                    });
                }
            });
        }
    }
}

fn egui_execution_state(ui: &mut Ui, execution_state: &ExecutionState) {
    ui.vertical(|ui| {
        for (key, value) in execution_state.state.iter() {
            ui.horizontal(|ui| {
                ui.label(format!("{:?}", key));
                ui.separator();
                egui_rkyv(ui, value);
            });
        }
    });
}

pub fn lerp_ease_in_out(start: Vec3, end: Vec3, s: f32) -> Vec3 {
    let t = s * s * (3.0 - 2.0 * s); // Cubic Hermite (smoothstep) easing
    start + ((end - start) * t)
}

fn camera_follow_exec_head(
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx),
        (With<GraphIdx>, Without<GraphIdxPair>, Without<Camera>),
    >,
    mut q_camera: Query<(&Camera, &mut Transform), With<crate::graph::OnGraphScreen>>,
    execution_graph: ResMut<crate::chidori::ChidoriExecutionGraph>,
) {
    let (camera, mut camera_transform) = q_camera.single_mut();
    node_query
        .iter_mut()
        .for_each(|(entity, mut transform, graph_idx)| {
            if execution_graph.current_execution_head == graph_idx.execution_id {
                camera_transform.translation = camera_transform.translation.lerp(transform.translation, 0.8);

            }
        });
}

fn mouse_over_system(
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
    q_camera: Query<(&Camera, &GlobalTransform), With<OnGraphScreen>>,
    internal_state: ResMut<crate::chidori::InternalState>,
    exec_id_to_state: ResMut<crate::chidori::ChidoriExecutionIdsToStates>,
) {
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
            if let Some(state) = exec_id_to_state.inner.get(&gidx.execution_id) {
                gidx.loading = false;
                if let Some(pos) = camera.world_to_viewport(camera_transform, t.translation) {
                    egui::containers::popup::show_tooltip_at_pointer(
                        ctx,
                        egui::Id::new("my_tooltip"),
                        |ui| {
                            ui.label(format!("{:?}", gidx.execution_id));
                            egui_execution_state(ui, state)
                        },
                    );
                }
            } else {
                if gidx.loading {
                    if let Some(pos) = camera.world_to_viewport(camera_transform, t.translation) {
                        egui::containers::popup::show_tooltip_at_pointer(
                            ctx,
                            egui::Id::new("my_tooltip"),
                            |ui| {
                                ui.label("Loading...");
                            },
                        );
                    }
                } else {
                    internal_state.get_execution_state_at_id(gidx.execution_id);
                    gidx.loading = true;
                    if let Some(pos) = camera.world_to_viewport(camera_transform, t.translation) {
                        egui::containers::popup::show_tooltip_at_pointer(
                            ctx,
                            egui::Id::new("my_tooltip"),
                            |ui| {
                                ui.label("Loading...");
                            },
                        );
                    }
                }
            }

            gidx.is_hovered = true;

            if buttons.just_pressed(MouseButton::Left) {
                internal_state.set_execution_id(gidx.execution_id);
                selected_entity.id = Some(entity);
            }
        }

        false
    });
}

fn node_coloring_handling(
    mut commands: Commands,
    selected_entity: Res<SelectedEntity>,
    mut node_query: Query<
        (Entity, &mut Handle<StandardMaterial>, &GraphIdx),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut materials: ResMut<Assets<StandardMaterial>>,
    execution_graph: ResMut<crate::chidori::ChidoriExecutionGraph>,
) {
    node_query
        .iter_mut()
        .for_each(|(entity, mut material, graph_idx)| {
            if execution_graph.current_execution_head == graph_idx.execution_id {
                *material = materials.add(StandardMaterial {
                    base_color: Color::hex("#ff0000").unwrap().into(),
                    unlit: true,
                    ..default()
                });
                return;
            } else {
                if Some(entity) == selected_entity.id {
                    *material = materials.add(StandardMaterial {
                        base_color: Color::hex("#00ff00").unwrap().into(),
                        unlit: true,
                        ..default()
                    });
                    return;
                }
            }
            *material = materials.add(StandardMaterial {
                base_color: Color::hex("#ffffff").unwrap().into(),
                unlit: true,
                ..default()
            });
        });
}

#[derive(Resource, Default)]
struct NodeIdToEntity {
    mapping: HashMap<NodeIndex, Entity>,
}

fn update_alternate_graph_system(
    mut commands: Commands,
    mut graph_res: ResMut<GraphResource>,
    mut gizmos: Gizmos,
    mut node_id_to_entity: ResMut<NodeIdToEntity>,
    mut node_query: Query<
        (Entity, &mut Transform, &GraphIdx),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let mut topo = petgraph::visit::Topo::new(&graph_res.graph);
    let mut node_mapping: HashMap<NodeIndex, NonNull<crate::tidy_tree::Node>> = HashMap::new();
    let mut tidy = TidyLayout::new_layered(10., 10.);
    let mut root = crate::tidy_tree::Node::new(0, 10., 10.);
    while let Some(x) = topo.next(&graph_res.graph) {
        if let Some(node) = &graph_res.graph.node_weight(x) {
            let tree_node = crate::tidy_tree::Node::new(x.index(), 20., 20.);
            let mut parents = &mut graph_res
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

    let mut topo = petgraph::visit::Topo::new(&graph_res.graph);
    while let Some(idx) = topo.next(&graph_res.graph) {
        if let Some(node) = &graph_res.graph.node_weight(idx) {
            let mut parents = &mut graph_res
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
                        let entity = commands.spawn((
                            PbrBundle {
                                mesh: meshes.add(Mesh::from(Circle { radius: 10.0 })),
                                material: materials.add(StandardMaterial {
                                    base_color: Color::hex("#ffffff").unwrap().into(),
                                    unlit: true,
                                    ..default()
                                }),
                                transform: Transform::from_xyz(parent_pos.x, parent_pos.y, -30.0),
                                ..Default::default()
                            },
                            GraphIdx {
                                loading: false,
                                execution_id: node.0,
                                id: idx.index(),
                                is_hovered: false,
                            },
                            Collider::ball(10.0),
                            RenderLayers::layer(2),
                            // OnGraphScreen
                        ));
                        entity.id()
                    });

                    if let Ok((_, mut transform, _)) = node_query.get_mut(*entity) {
                        transform.translation = transform.translation.lerp(Vec3::new(n.x.to_f32().unwrap(), -n.y.to_f32().unwrap(), -30.0),
                                                       0.8);
                    }

                    n.children.iter().for_each(|child| {
                        let parent = n;
                        let child = child.as_ref();
                        gizmos.line(
                            Vec3::new(parent.x.to_f32().unwrap(), -parent.y.to_f32().unwrap(), -30.0),
                            Vec3::new(child.x.to_f32().unwrap(), -child.y.to_f32().unwrap(), -30.0),
                            Color::WHITE,
                        );
                    });
                }
            }
        }
    }
}

// We use immediate mode rendering to draw the graph in order to avoid the complexity
// of mutating a mesh and passing it back and forth to the GPU as we move the lines around
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

fn set_camera_viewports(
    windows: Query<&Window>,
    mut tree: ResMut<EguiTree>,
    mut resize_events: EventReader<WindowResized>,
    mut main_camera: Query<(&mut Camera, &mut Projection), (With<OnGraphScreen>,)>,
) {

    tree.tree.tiles.iter().for_each(|(_, tile)| {
        match tile {
            Tile::Pane(p) => {
                if &p.nr == &"Graph" {
                    if let Some(r) = p.rect {
                        let (mut camera, mut projection) = main_camera.single_mut();
                        let viewport = Viewport {
                            physical_position: UVec2::new(r.min.x as u32, r.min.y as u32),
                            physical_size: UVec2::new(r.width() as u32, r.height() as u32),
                            depth: Default::default(),
                        };
                        camera.viewport = Some(viewport);
                    }
                }
            }
            Tile::Container(_) => {}
        }
    });
    // We need to dynamically resize the camera's viewports whenever the window size changes
    // so then each camera always takes up half the screen.
    // A resize_event is sent when the window is first created, allowing us to reuse this system for initial setup.
    for resize_event in resize_events.read() {

    }
}


fn graph_setup(
    windows: Query<&Window>,
    mut commands: Commands,
    mut execution_graph: ResMut<ChidoriExecutionGraph>,
    mut config_store: ResMut<GizmoConfigStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials_color: ResMut<Assets<ColorMaterial>>,
    mut materials_standard: ResMut<Assets<StandardMaterial>>,
) {
    let window = windows.single();

    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 6,
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 1.0)
                .looking_at(Vec3::ZERO, Vec3::Y),
            projection: OrthographicProjection {
                scale: 1.0,
                ..default()
            }
            .into(),
            ..default()
        },
        OnGraphScreen,
        RenderLayers::layer(2),
    ));

    let entity = commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(Circle { radius: 10.0 })),
            material: materials_standard.add(StandardMaterial {
                base_color: Color::hex("#00ffff").unwrap().into(),
                unlit: true,
                ..default()
            }),
            transform: Transform::from_xyz(0.0, 0.0, -20.0),
            ..Default::default()
        },
        RenderLayers::layer(2),
        ExecutionHeadCursor,
        OnGraphScreen
    ));

    let entity = commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(Circle { radius: 10.0 })),
            material: materials_standard.add(StandardMaterial {
                base_color: Color::hex("#0000ff").unwrap().into(),
                unlit: true,
                ..default()
            }),
            transform: Transform::from_xyz(0.0, 0.0, -20.0),
            ..Default::default()
        },
        RenderLayers::layer(2),
        ExecutionSelectionCursor,
        OnGraphScreen
    ));


    let (config, _) = config_store.config_mut::<DefaultGizmoConfigGroup>();
    config.line_width = 1.0;
    config.render_layers = RenderLayers::layer(2);

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
    commands.spawn((CursorWorldCoords(vec2(0.0, 0.0)), OnGraphScreen));
    commands.insert_resource(GraphResource {
        graph,
        hash_graph: hash_graph(&execution_graph.inner),
        node_ids,
    });
}

#[derive(Component)]
struct OnGraphScreen;

pub fn graph_plugin(app: &mut App) {
    app.init_resource::<NodeIdToEntity>()
        .init_resource::<SelectedEntity>()
        .add_systems(OnEnter(crate::GameState::Graph), graph_setup)
        .add_systems(
            OnExit(crate::GameState::Graph),
            despawn_screen::<OnGraphScreen>,
        )
        .add_systems(
            Update,
            (
                set_camera_viewports,
                camera_follow_exec_head,
                node_coloring_handling,
                touchpad_gestures,
                update_alternate_graph_system,
                update_graph_system,
                my_cursor_system,
                mouse_scroll_events,
                mouse_over_system,
            )
                .run_if(in_state(GameState::Graph)),
        );
}
