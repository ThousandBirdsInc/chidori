use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use bevy::app::{App, Update};
use bevy_rapier2d::prelude::*;
use bevy::asset::Asset;
use bevy::math::{vec2, Vec3};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::prelude::{Assets, Camera2dBundle, Circle, Color, ColorMaterial, Commands, Component, default, in_state, IntoSystemConfigs, MaterialMeshBundle, Mesh, OnEnter, OnExit, ResMut, Transform, TypePath};
use bevy::render::mesh::{PrimitiveTopology, MeshVertexBufferLayout, VertexAttributeValues};
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{AsBindGroup, PolygonMode, RenderPipelineDescriptor, ShaderRef, SpecializedMeshPipelineError};
use bevy::render::view::RenderLayers;
use bevy::sprite::{MaterialMesh2dBundle, Mesh2dHandle};
use bevy::tasks::futures_lite::StreamExt;
use bevy::utils::petgraph::stable_graph::GraphIndex;
use bevy::window::PrimaryWindow;
use bevy_cosmic_edit::{CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem};
use bevy_rapier2d::geometry::Collider;
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use fdg::{fruchterman_reingold::FruchtermanReingold, Force, ForceGraph, simple::Center};
use fdg::nalgebra::{Const, OVector, Point};
use fdg::petgraph::graph::NodeIndex;
use petgraph::adj::DefaultIx;
use petgraph::Directed;
use petgraph::graph::DiGraph;
use petgraph::prelude::{DiGraphMap, StableGraph};
use chidori_core::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId};
use chidori_core::sdk::entry::Chidori;
use crate::chidori::ChidoriExecutionGraph;
use crate::GameState;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text};



#[derive(Resource)]
struct GraphResource {
    graph: ForceGraph<f32, 2, ExecutionNodeId, ()>,
    hash_graph: u64,
    node_ids: HashMap<ExecutionNodeId, NodeIndex>
}

#[derive(Component)]
struct GraphIdx{
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


fn my_cursor_system(
    mut q_mycoords: Query<&mut CursorWorldCoords, With<OnGraphScreen>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Camera, &GlobalTransform), With<OnGraphScreen>>,
) {
    let mut coords = q_mycoords.single_mut();
    let (camera, camera_transform) = q_camera.single();
    let window = q_window.single();
    if let Some(world_position) = window.cursor_position()
        .and_then(|cursor| camera.viewport_to_world(camera_transform, cursor))
        .map(|ray| ray.origin.truncate())
    {
        coords.0 = world_position;
    }
}


fn mouse_over_system(
    mut commands: Commands,
    q_mycoords: Query<&CursorWorldCoords, With<OnGraphScreen>>,
    mut node_query: Query<(Entity, &Collider, &mut GraphIdx), (With<GraphIdx>, Without<GraphIdxPair>)>,
    mut gizmos: Gizmos,
    rapier_context: Res<RapierContext>,
) {
    let cursor = q_mycoords.single();

    for (_, collider, mut gidx) in node_query.iter_mut() {
        gidx.is_hovered = false;
    }

    // gizmos
    //     .circle(Vec3::new(cursor.0.x, cursor.0.y, 0.0), Direction3d::Z, 1.0, Color::YELLOW)
    //     .segments(64);
    let point = Vec2::new(cursor.0.x, cursor.0.y);
    let filter = QueryFilter::default();
    rapier_context.intersections_with_point(
        point, filter, |entity| {
            if let Ok((_, _, mut gidx)) = node_query.get_mut(entity) {
                gidx.is_hovered = true;
            }
            false
        }
    );
}

// We use immediate mode rendering to draw the graph in order to avoid the complexity
// of mutating a mesh and passing it back and forth to the GPU as we move the lines around
fn update_graph_system(
    mut commands: Commands,
    mut graph_res: ResMut<GraphResource>,
    mut execution_graph: ResMut<ChidoriExecutionGraph>,
    mut node_query: Query<(Entity, &mut Transform, &GraphIdx), (With<GraphIdx>, Without<GraphIdxPair>)>,
    mut gizmos: Gizmos,
    mut line_query: Query<(Entity, &GraphIdxPair), (With<GraphIdxPair>, Without<GraphIdx>)>,
) {
    // If the execution graph has changed, clear the graph and reconstruct it
    if graph_res.hash_graph != hash_graph(&execution_graph.inner) {
        let mut dataset = StableGraph::new();
        let mut node_ids = HashMap::new();
        for (a, b) in &execution_graph.inner {
            let node_index_a = *node_ids.entry(a.clone()).or_insert_with(|| dataset.add_node(a.clone()));
            let node_index_b = *node_ids.entry(b.clone()).or_insert_with(|| dataset.add_node(b.clone()));
            dataset.add_edge(node_index_a, node_index_b, ());
        }
        let mut graph: ForceGraph<f32, 2, ExecutionNodeId, ()> = fdg::init_force_graph_uniform(dataset, 30.0);
        FruchtermanReingold::default().apply(&mut graph);
        Center::default().apply(&mut graph);

        for idx in graph.node_indices() {
            if !node_query.iter().any(|(_, _, graph_idx)| graph_idx.id == idx.index()) {
                commands.spawn((
                    GraphIdx{ id: idx.index(), is_hovered: false },
                    TransformBundle::from(Transform::from_xyz(0.0, 0.0, 0.0)),
                    Collider::ball(12.0),
                    OnGraphScreen));
            }
        }

        for edge_idx in graph.edge_indices() {
            let (source_idx, target_idx) = graph.edge_endpoints(edge_idx).unwrap();

            if !line_query.iter().any(|(_, idx_pair)| idx_pair.source == source_idx.index() && idx_pair.target == target_idx.index()) {
                commands.spawn((GraphIdxPair {
                    source: source_idx.index(),
                    target: target_idx.index(),
                }, OnGraphScreen ));
            }
        }
        graph_res.node_ids = node_ids;
        graph_res.graph = graph;
        graph_res.hash_graph = hash_graph(&execution_graph.inner);
    }


    // Apply the graph algorithm
    let mut f = FruchtermanReingold::default();
    f.conf.scale = 80.0;
    f.apply(&mut graph_res.graph);

    // Update entity positions based on the graph data
    for (_, mut transform, graph_idx) in node_query.iter_mut() {
        if let Some((_, pos)) = graph_res.graph.node_weight(NodeIndex::new(graph_idx.id)) {
            transform.translation = Vec3::new(pos.x, pos.y, 0.0);
            let color = if graph_idx.is_hovered { Color::GREEN } else { Color::WHITE };
            gizmos
                .circle(Vec3::new(pos.x, pos.y, 0.0), Direction3d::Z, 10.0, color)
                .segments(64);
        }
    }

    // Update line positions based on updated node positions
    for (_, idx_pair) in line_query.iter_mut() {
        if let Some((_, source_pos)) = graph_res.graph.node_weight(NodeIndex::new(idx_pair.source)) {
            if let Some((_, target_pos)) = graph_res.graph.node_weight(NodeIndex::new(idx_pair.target)) {
                gizmos.line(
                    Vec3::new(source_pos.x, source_pos.y, 0.0),
                    Vec3::new(target_pos.x, target_pos.y, 0.0),
                    Color::WHITE,
                );
            }
        }
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

fn graph_setup(
    mut commands: Commands,
    mut execution_graph: ResMut<ChidoriExecutionGraph>,
    mut config_store: ResMut<GizmoConfigStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    commands.spawn((Camera3dBundle {
        transform: Transform::from_xyz(0.0, 0.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
        projection: OrthographicProjection {
            scale: 0.8,
            ..default()
        }.into(),
        ..default()
    }, OnGraphScreen, RenderLayers::layer(2)));

    let (config, _) = config_store.config_mut::<DefaultGizmoConfigGroup>();
    config.line_width = 1.0;
    config.render_layers = RenderLayers::layer(2);

    let mut rng = rand::thread_rng();
    let mut dataset = StableGraph::new();
    let mut node_ids = HashMap::new();
    for (a, b) in &execution_graph.inner {
        let node_index_a = *node_ids.entry(a.clone()).or_insert_with(|| dataset.add_node(a.clone()));
        let node_index_b = *node_ids.entry(b.clone()).or_insert_with(|| dataset.add_node(b.clone()));
        dataset.add_edge(node_index_a, node_index_b, ());
    }
    let mut graph: ForceGraph<f32, 2, ExecutionNodeId, ()> = fdg::init_force_graph_uniform(dataset, 30.0);
    FruchtermanReingold::default().apply(&mut graph);
    Center::default().apply(&mut graph);

    for idx in graph.node_indices() {
        commands.spawn((
            GraphIdx{ id: idx.index(), is_hovered: false },
            TransformBundle::from(Transform::from_xyz(0.0, 0.0, 0.0)),
            Collider::ball(12.0),
            Sensor,
            OnGraphScreen));
    }

    for edge_idx in graph.edge_indices() {
        let (source_idx, target_idx) = graph.edge_endpoints(edge_idx).unwrap();

        // Spawn a list of lines with start and end points for each lines
        commands.spawn((GraphIdxPair {
            source: source_idx.index(),
            target: target_idx.index(),
        }, OnGraphScreen ));
    }
    commands.spawn((CursorWorldCoords(vec2(0.0,0.0)), OnGraphScreen));
    commands.insert_resource(GraphResource {
        graph,
        hash_graph: hash_graph(&execution_graph.inner),
        node_ids
    });
}

#[derive(Component)]
struct OnGraphScreen;

pub fn graph_plugin(app: &mut App) {
    app
        .add_systems(Update, (update_graph_system, my_cursor_system, mouse_over_system).run_if(in_state(GameState::Graph)))
        .add_systems(OnEnter(crate::GameState::Graph), graph_setup)
        .add_systems(OnExit(crate::GameState::Graph), despawn_screen::<OnGraphScreen>);
}
