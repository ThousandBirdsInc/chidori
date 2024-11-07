//! A shader that renders a mesh multiple times in one draw call.

use std::collections::HashMap;
use std::num::NonZero;
use bevy::input::touchpad::TouchpadMagnify;
use std::ops::Add;
use std::time::{Duration, Instant};
use bevy::{ prelude::*, render::{ extract_component::ExtractComponent , render_phase::{ PhaseItem, RenderCommand , } , render_resource::* , view::NoFrustumCulling, }, };
use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::math::{vec2, vec3};
use bevy::render::camera::{ScalingMode, Viewport};
use bevy::render::view::RenderLayers;
use bevy::sprite::Anchor;
use bevy::text::{BreakLineOn, Text2dBounds};
use petgraph::visit::Walker;
use bevy::window::{PrimaryWindow, WindowResized};
use crate::bevy_egui::{egui, EguiContexts};
use bevy_rapier2d::geometry::{Collider, Sensor};
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use egui_tiles::Tile;
use petgraph::graph::DiGraph;
use petgraph::prelude::{EdgeRef, NodeIndex, StableDiGraph, StableGraph};
use chidori_core::utils::telemetry::TraceEvents;
use crate::chidori::{ChidoriState, EguiTree, EguiTreeIdentities};
use crate::{RENDER_LAYER_TRACE_MINIMAP, RENDER_LAYER_TRACE_TEXT, RENDER_LAYER_TRACE_VIEW};
use crate::shader_trace::{CustomMaterialPlugin, InstanceData, InstanceMaterialData};
use crate::util::despawn_screen;


const RENDER_TEXT: bool = true;
const HANDLE_COLLISIONS: bool = true;
const SPAN_HEIGHT: f32 = 28.0;
const CAMERA_SPACE_WIDTH: f32 = 1000.0;
const MINIMAP_OFFSET: u32 = 0;
const MINIMAP_HEIGHT: u32 = 100;
const MINIMAP_HEIGHT_AND_OFFSET: u32 = MINIMAP_OFFSET + MINIMAP_HEIGHT;

#[derive(Component)]
struct MinimapTraceViewportIndicator;

#[derive(Component)]
struct IdentifiedSpan {
    node_idx: NodeIndex,
    id: String,
    is_hovered: bool,
}

#[derive(Resource, Debug)]
struct TraceSpaceViewport {
    x: f32,
    y: f32,
    horizontal_scale: f32, // scale of the view
    vertical_scale: f32,
    max_vertical_extent: f32,
    is_active: bool
}

fn update_trace_space_to_minimap_camera_configuration(
    trace_space: Res<TraceSpaceViewport>,
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnTraceScreen>, With<TraceCameraMinimap>)>,
) {
    let (projection, mut camera_transform) = camera.single_mut();
    let (mut scale) = match projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { (&mut o.scaling_mode) }
    };
    camera_transform.translation.y = -trace_space.max_vertical_extent / 2.0;
    *scale = ScalingMode::Fixed {
        width: CAMERA_SPACE_WIDTH,
        height: trace_space.max_vertical_extent,
    };
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

fn triangle_wave(x: f32) -> f32 {
    2.0 * (fract(x) - 0.5).abs() - 1.0
}

fn color_for_bucket(t: f32, a: f32) -> Color {
    let C_0 = 0.2;
    let C_d = 0.1;
    let L_0 = 0.2;
    let L_d = 0.1;
    let x = triangle_wave(10.0 * t);
    let H = 360.0 * (0.9 * t);
    let C = C_0 + C_d * x;
    let L = L_0 - L_d * x;
    Color::Lcha {
        lightness: L,
        chroma:C,
        hue:H,
        alpha: a,
    }
}



fn update_trace_space_to_camera_configuration(
    windows: Query<&Window>,
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut main_camera: Query<(&mut Projection, &mut Transform), (With<TraceCameraTraces>, Without<TraceCameraMinimap>, Without<TraceCameraTextAtlas>)>,
    mut minimap_camera: Query<(&mut Projection, &mut Transform), (With<TraceCameraMinimap>, Without<TraceCameraTraces>, Without<TraceCameraTextAtlas>)>,
    mut minimap_trace_viewport_indicator: Query<(&mut Transform), (With<MinimapTraceViewportIndicator>, Without<TraceCameraTraces>, Without<TraceCameraMinimap>)>,
) {

    let window = windows.single();
    let scale_factor = window.scale_factor();
    let (trace_projection, mut trace_camera_transform) = main_camera.single_mut();
    let (mini_projection, mut mini_camera_transform) = minimap_camera.single_mut();

    let trace_projection = match trace_projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { o }
    };
    let mini_projection = match mini_projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { o }
    };

    trace_projection.scaling_mode = ScalingMode::Fixed {
        width: trace_space.horizontal_scale,
        height: trace_space.vertical_scale,
    };

    let camera_position = mini_camera_transform.translation;
    let trace_viewport_width = trace_projection.area.width();
    let trace_viewport_height = trace_projection.area.height();
    let viewport_width = mini_projection.area.width();

    let left = camera_position.x - viewport_width / 2.0 + (trace_viewport_width / 2.0);
    let right = camera_position.x + viewport_width / 2.0 - (trace_viewport_width / 2.0);
    let top = 0.0;
    let bottom = (-trace_space.max_vertical_extent + (trace_viewport_height)).min(0.0);

    trace_space.x = trace_space.x.clamp(left, right);
    trace_space.y = trace_space.y.clamp(bottom, top);

    trace_camera_transform.translation.x = trace_space.x;
    trace_camera_transform.translation.y = trace_space.y - (trace_space.vertical_scale * 0.5);
    minimap_trace_viewport_indicator.iter_mut().for_each(|mut transform| {
        transform.translation.x = trace_space.x;
        transform.translation.y = trace_camera_transform.translation.y;
        transform.scale.x = trace_space.horizontal_scale;
        transform.scale.y = trace_space.vertical_scale;
    });
}



fn mouse_scroll_events(
    mut scroll_evr: EventReader<MouseWheel>,
    mut trace_space: ResMut<TraceSpaceViewport>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    for ev in scroll_evr.read() {
        if keyboard_input.pressed(KeyCode::SuperLeft) {
            trace_space.horizontal_scale = (trace_space.horizontal_scale + ev.y).clamp(1.0, 1000.0);
        } else {
            trace_space.x -= (ev.x * (trace_space.horizontal_scale / 1000.0));
            trace_space.y += ev.y;
        }
    }
}


#[derive(Component, Default)]
struct CursorWorldCoords(Vec2);




fn my_cursor_system(
    mut q_mycoords: Query<&mut CursorWorldCoords, With<OnTraceScreen>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Camera, &GlobalTransform), (With<OnTraceScreen>, With<TraceCameraTextAtlas>)>,
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

fn mouse_pan(
    mut trace_space: ResMut<TraceSpaceViewport>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion_evr: EventReader<MouseMotion>,
) {
    if buttons.pressed(MouseButton::Left) {
        for ev in motion_evr.read() {
            trace_space.x -= ev.delta.x;
        }
    }
}


// these only work on macOS
fn touchpad_gestures(
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut evr_touchpad_magnify: EventReader<TouchpadMagnify>,
) {
    for ev_magnify in evr_touchpad_magnify.read() {
        trace_space.horizontal_scale = (trace_space.horizontal_scale + (ev_magnify.0 * trace_space.horizontal_scale)).clamp(1.0, 1000.0);
    }
}

fn mouse_over_system(
    mut trace_space: ResMut<TraceSpaceViewport>,
    q_mycoords: Query<&CursorWorldCoords, With<OnTraceScreen>>,
    mut node_query: Query<(Entity, &Collider, &mut IdentifiedSpan), (With<IdentifiedSpan>, Without<MinimapTraceViewportIndicator>)>,
    mut minimap_trace_viewport_indicator: Query<(Entity, &Collider, &Handle<StandardMaterial>), (With<MinimapTraceViewportIndicator>, Without<IdentifiedSpan>)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut gizmos: Gizmos,
    rapier_context: Res<RapierContext>,
    mut contexts: EguiContexts,
    call_tree: Res<TracesCallTree>,
    last_click: Local<Option<Instant>>
) {
    let double_click_threshold = Duration::from_millis(500);

    if !trace_space.is_active {
        return;
    }

    let ctx = contexts.ctx_mut();
    let cursor = q_mycoords.single();

    for (_, collider, mut span) in node_query.iter_mut() {
        span.is_hovered = false;
    }

    for (_, _, material_handle) in minimap_trace_viewport_indicator.iter_mut() {
        if let Some(mut mat) = materials.get_mut(material_handle) {
            mat.base_color = Color::hsla(3.0, 1.0, 1.0, 0.8);
        }
    }

    gizmos
        .circle(Vec3::new(cursor.0.x, cursor.0.y, 0.0), Direction3d::Z, 1.0, Color::YELLOW)
        .segments(64);
    let point = Vec2::new(cursor.0.x, cursor.0.y);
    let filter = QueryFilter::default();
    rapier_context.intersections_with_point(
        point, filter, |entity| {
            if let Ok((_, _, mut span)) = node_query.get_mut(entity) {
                span.is_hovered = true;
                egui::containers::popup::show_tooltip_at_pointer(ctx, egui::LayerId::new(Order::Foreground, egui::Id::new("my_tooltiplayer")), egui::Id::new("my_tooltip"), |ui| {
                    call_tree.inner.graph.node_weight(span.node_idx).map(|node| {
                        match &node.event {
                            TraceEvents::NewSpan {name, location, line, thread_id, parent_id, execution_id, ..} => {
                                ui.label(format!("{:?}", node.id));
                                ui.label(format!("name: {}", name));
                                ui.label(format!("location: {}", location));
                                ui.label(format!("line: {}", line));
                                ui.label(format!("thread_id: {}", thread_id));
                                ui.label(format!("parent_id: {:?}", parent_id));
                                ui.label(format!("absolute_timestamp: {:?}", node.absolute_timestamp));
                                ui.label(format!("parent_relative_timestamp: {:?}", node.adjusted_timestamp));
                                ui.label(format!("total_duration: {:.2} seconds", (node.total_duration as f64) / 1_000_000_000.0));
                                ui.label(format!("total_duration {:?}", node.total_duration));
                                ui.label(format!("depth {:?}", node.depth));
                                if let Some(execution_id) = execution_id {
                                    ui.label(format!("execution_id: {:?}", execution_id));
                                }
                            }
                            _ => {}
                        }
                    });
                });
            }

            if let Ok((_, _, material_handle)) = minimap_trace_viewport_indicator.get_mut(entity) {
                if let Some(mut mat) = materials.get_mut(material_handle) {
                    mat.base_color = Color::hsla(3.0, 1.0, 1.0, 0.5);
                }
            }

            false
        }
    );
}

#[derive(Debug, Clone)]
struct CallNode {
    id: String,
    created_at: Instant,
    depth: usize,
    thread_depth: usize,
    render_lane: usize,
    adjusted_timestamp: u128,
    absolute_timestamp: u128,
    total_duration: u128,
    event: TraceEvents,
    color_bucket: f32,
}

#[derive(Debug)]
struct CallTree {
    max_thread_depth: usize,
    max_render_lane: usize,
    relative_endpoint: u128,
    startpoint: u128,
    endpoint: u128,
    graph: DiGraph<CallNode, ()>,
}

impl Default for CallTree {
    fn default() -> Self {
        Self {
            max_thread_depth: 0,
            max_render_lane: 0,
            startpoint: 0,
            endpoint: 0,
            relative_endpoint: 0,
            graph: DiGraph::new()
        }
    }

}




fn build_call_tree(events: Vec<TraceEvents>, collapse_gaps: bool) -> CallTree {
    let mut node_map: HashMap<String, (u128, NonZero<u64>, NodeIndex)> = HashMap::new();
    let mut graph: DiGraph<CallNode, ()> = DiGraph::new();
    let mut max_thread_depth = 1;
    let mut max_render_lane = 0;
    let mut endpoint = 0;
    let mut relative_endpoint = 0;
    let mut startpoint = u128::MAX;
    let mut render_lanes: HashMap<String, Vec<(u128, u128)>> = HashMap::new(); // Track occupied time ranges for each parent span
    let mut last_top_level_trace_end = 0;

    for event in events {
        match &event {
            e @ TraceEvents::NewSpan {
                id,
                parent_id,
                weight,
                thread_id,
                created_at,
                ..
            } => {
                let weight = *weight;
                let mut node = CallNode {
                    id: id.clone(),
                    created_at: created_at.clone(),
                    depth: 1,
                    thread_depth: 1,
                    render_lane: 1,
                    adjusted_timestamp: weight,
                    absolute_timestamp: weight,
                    total_duration: 0,
                    event: e.clone(),
                    color_bucket: 0.,
                };

                if weight < startpoint {
                    startpoint = weight;
                }
                if weight > endpoint {
                    endpoint = weight;
                }

                // Assign depth and thread_depth
                if let Some(parent_id) = parent_id {
                    if let Some((_, _, parent_idx)) = node_map.get(parent_id) {
                        if let Some(parent) = graph.node_weight(*parent_idx) {
                            node.depth = parent.depth + 1;
                            if let TraceEvents::NewSpan {thread_id: parent_thread_id, ..} = parent.event {
                                if thread_id != &parent_thread_id {
                                    node.thread_depth = parent.thread_depth + 1;
                                } else {
                                    node.thread_depth = parent.thread_depth;
                                }
                            }
                            max_thread_depth = max_thread_depth.max(node.thread_depth);
                        }
                    }
                }

                // Assign render_lane for non-overlapping rendering
                let parent_key = parent_id.clone().unwrap_or_else(|| "root".to_string());
                let parent_lanes = render_lanes.entry(parent_key).or_insert_with(Vec::new);

                // Find the first available lane within the parent's span
                let mut available_lane = 1;
                'outer: loop {
                    for &(start, end) in parent_lanes.iter() {
                        if weight >= start && weight < end {
                            available_lane += 1;
                            continue 'outer;
                        }
                    }
                    break;
                }

                node.render_lane = available_lane;
                parent_lanes.push((weight, weight)); // We'll update the end time when we process the Exit event

                max_render_lane = max_render_lane.max(node.render_lane);


                let mut position_subtracted_by_x_amount: u128 = 0;
                // If this is a child node, adjusted position is its absolute position - the parent's adjustment
                if let Some(parent_id) = parent_id {
                    if let Some((parent_adjustment_to_position, _, _)) = node_map.get(parent_id) {
                        position_subtracted_by_x_amount = *parent_adjustment_to_position;
                        node.adjusted_timestamp = weight - parent_adjustment_to_position;
                    }
                }

                // If this is a top-level node, adjust its position to the end of the last top-level node
                // If there is no completed top-level node, its adjustment is the start_time
                if parent_id.is_none() {
                    if weight >= last_top_level_trace_end {
                        position_subtracted_by_x_amount = weight - last_top_level_trace_end;
                    } else {
                        position_subtracted_by_x_amount = 0;
                    }
                    node.adjusted_timestamp = last_top_level_trace_end;
                }

                relative_endpoint = relative_endpoint.max(node.adjusted_timestamp);

                // Add node to graph and node_map
                let node_idx = graph.add_node(node);
                node_map.insert(id.clone(), (position_subtracted_by_x_amount, *thread_id, node_idx));

                // node_map.insert(id.clone(), node_idx);

                // Add edge between parent and child
                if let Some(parent_id) = parent_id {
                    if let Some((_, _, parent_idx)) = node_map.get(parent_id) {
                        graph.add_edge(*parent_idx, node_idx, ());
                    }
                }
            }
            TraceEvents::Exit(id, weight) => {
                if let Some((_, _, node_idx)) = node_map.get(id) {
                    if let Some(node) = graph.node_weight_mut(*node_idx) {
                        node.total_duration = weight - node.absolute_timestamp;
                        let endpoint_absolute = node.absolute_timestamp + node.total_duration;
                        let endpoint_adjusted = node.adjusted_timestamp + node.total_duration;

                        // If this is a top-level node, update the last top-level trace end
                        if node.depth == 1 {
                            last_top_level_trace_end = endpoint_adjusted;
                        }

                        // Update render lane end time
                        if let TraceEvents::NewSpan { parent_id: event_parent_id, ..} = &node.event {
                            if let Some(parent_id) = event_parent_id {
                                if let Some(lanes) = render_lanes.get_mut(parent_id) {
                                    if let Some(lane) = lanes.get_mut(node.render_lane) {
                                        lane.1 = *weight;
                                    }
                                }
                            } else if let Some(lanes) = render_lanes.get_mut("root") {
                                if let Some(lane) = lanes.get_mut(node.render_lane) {
                                    lane.1 = *weight;
                                }
                            }
                        }

                        relative_endpoint = relative_endpoint.max(endpoint_adjusted);
                        endpoint = endpoint.max(endpoint_absolute);
                    }
                }
            }
            _ => {}
        }
    }

    // Assign color buckets (unchanged)
    let mut vec_keys: Vec<_> = graph.node_indices().collect();
    vec_keys.sort_by_key(|&idx| {
        if let Some(node) = graph.node_weight(idx) {
            match &node.event {
                TraceEvents::NewSpan { name, location, .. } => format!("{}{}", location, name),
                _ => String::new(),
            }
        } else {
            String::new()
        }
    });

    for (i, idx) in vec_keys.iter().enumerate() {
        if let Some(node) = graph.node_weight_mut(*idx) {
            node.color_bucket = (i as f32) / (vec_keys.len() as f32);
        }
    }

    // Set durations for incomplete traces
    let now = Instant::now();
    for node in graph.node_weights_mut() {
        if node.total_duration == 0 {
            node.total_duration = (now - node.created_at).as_nanos();
        }
    }

    CallTree {
        max_thread_depth,
        max_render_lane,
        relative_endpoint,
        startpoint,
        endpoint,
        graph,
    }
}

fn scale_to_target(v: u128, max_value: u128, target_max: f32) -> f32 {
    let scale_factor = target_max / max_value as f32;
    v as f32 * scale_factor
}

fn unscale_from_target(v: f32, max_value: u128, target_max: f32) -> u128 {
    let unscale_factor = max_value as f32 / target_max as f32;
    (v * unscale_factor) as u128
}


#[derive(Resource)]
struct SpanToTextMapping {
    spans: HashMap<String, Entity>,
    identity: HashMap<String, Entity>,
}


fn calculate_step_size(left: u128, right: u128, steps: u128) -> u128 {
    let interval_count = steps - 1;
    let raw_step_size = (right - left) / interval_count;
    let magnitude = 10_i64.pow(raw_step_size.to_string().len() as u32 - 1);
    let step_size = ((raw_step_size as f64 / magnitude as f64).round() * magnitude as f64);
    step_size as u128
}

#[derive(Resource, Default)]
struct TracesCallTree {
    inner: CallTree
}

fn maintain_call_tree(
    mut traces: ResMut<ChidoriState>,
    mut call_tree: ResMut<TracesCallTree>,
) {
    let tree = build_call_tree(traces.trace_events.clone(), false);
    call_tree.inner = tree;
}

fn update_positions(
    mut commands: Commands,
    mut gizmos: Gizmos,
    asset_server: Res<AssetServer>,
    mut instance_material_data: Query<(&mut InstanceMaterialData,)>,
    mut span_to_text_mapping: ResMut<SpanToTextMapping>,
    mut q_text_elements: Query<(&Text, &mut Text2dBounds, &mut Transform), With<Text>>,
    mut q_span_identities: Query<(&IdentifiedSpan, &mut Collider, &mut Transform), (With<IdentifiedSpan>, Without<Text>)>,
    trace_camera_query: Query<(&Camera, &Projection, &GlobalTransform), With<TraceCameraTraces>>,
    text_camera_query: Query<(&Camera, &GlobalTransform), With<TraceCameraTextAtlas>>,
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut call_tree: ResMut<TracesCallTree>,
    windows: Query<&Window>,
) {
    if !trace_space.is_active {
        return;
    }

    let scale_factor = windows.single().scale_factor();
    let span_height = SPAN_HEIGHT * scale_factor;
    let font = asset_server.load("fonts/CommitMono-1.143/CommitMono-400-Regular.otf");
    let text_style = TextStyle {
        font,
        font_size: 14.0,
        color: Color::WHITE,
    };

    let (trace_camera, trace_camera_projection, trace_camera_transform) = trace_camera_query.single();
    let (text_camera, text_camera_transform) = text_camera_query.single();

    let projection = match trace_camera_projection {
        Projection::Perspective(_) => {unreachable!("This should be orthographic")}
        Projection::Orthographic(o) => {o}
    };
    let camera_position = trace_camera_transform.translation();
    let viewport_width = projection.area.width();
    let viewport_height = projection.area.height();
    // left hand trace space coordinate
    let left = camera_position.x - viewport_width / 2.0;
    let right = camera_position.x + viewport_width / 2.0;

    let trace_to_text = |point: Vec3| -> Vec3 {
        let pos = trace_camera.world_to_ndc(trace_camera_transform, point).unwrap_or(Vec3::ZERO);
        text_camera.ndc_to_world(text_camera_transform, pos).unwrap_or(Vec3::ZERO)
    };

    let CallTree {
        relative_endpoint,
        startpoint: startpoint_value,
        endpoint: endpoint_value,
        graph: call_tree,
        ..
    } = &call_tree.inner;

    if relative_endpoint < startpoint_value {
        return;
    }

    // Render the increment markings
    if endpoint_value > startpoint_value {
        // Shift into positive values
        let left_time_space = unscale_from_target(left + CAMERA_SPACE_WIDTH / 2.0, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH);
        let right_time_space = unscale_from_target(right + CAMERA_SPACE_WIDTH / 2.0, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH);
        let step_size = calculate_step_size(left_time_space, right_time_space, 10).max(1);
        let mut movement = 0;
        while movement <= right_time_space {
            let x = scale_to_target(movement, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH) - (CAMERA_SPACE_WIDTH / 2.0);
            let source_pos = Vec3::new(x, viewport_height/2.0, 0.0);
            let target_pos = Vec3::new(x, -100000.0, 0.0);
            gizmos.line(
                trace_to_text(source_pos),
                trace_to_text(target_pos),
                Color::Rgba {
                    red: 1.0,
                    green: 1.0,
                    blue: 1.0,
                    alpha: 0.05,
                },
            );
            movement += step_size;
        }
    }

    for (mut data,) in instance_material_data.iter_mut() {
        let mut instances = data.0.iter_mut().collect::<Vec<_>>();
        let mut idx = 0;
        let root_nodes: Vec<NodeIndex> = call_tree.node_indices()
            .filter(|&node_idx| call_tree.edges_directed(node_idx, petgraph::Direction::Incoming).count() == 0)
            .collect();

        let mut max_vertical_extent : f32 = 0.0;
        for root_node in root_nodes {
            petgraph::visit::Dfs::new(&call_tree, root_node)
                .iter(&call_tree)
                .for_each(|node_idx| {
                    let node = call_tree.node_weight(node_idx).unwrap();
                    idx += 1;

                    // Filter rendering to only the currently viewed thread depth?

                    // Scaled to 1000.0 unit width, offset to move from centered to left aligned
                    // let config_space_pos_x = scale_to_target(node.absolute_timestamp - startpoint_value, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH) - (CAMERA_SPACE_WIDTH / 2.0);
                    let config_space_pos_x = scale_to_target(node.adjusted_timestamp - 0, relative_endpoint - startpoint_value, CAMERA_SPACE_WIDTH) - (CAMERA_SPACE_WIDTH / 2.0);
                    let config_space_width = scale_to_target(node.total_duration, relative_endpoint - startpoint_value, CAMERA_SPACE_WIDTH);
                    let screen_space_pos_y = ((node.depth as f32) * (node.render_lane as f32) * -1.0 * span_height + span_height / 2.0);
                    // let screen_space_pos_y = ((node.depth as f32) * (node.render_lane as f32) * -1.0 * span_height + span_height / 2.0) * node.thread_depth as f32;

                    // let screen_space_pos_y = ((node.depth as f32) * -1.0 * span_height + span_height / 2.0) * node.thread_depth as f32;
                    // let screen_space_pos_y = ((node.depth as f32) * -1.0 * span_height + span_height / 2.0) * 1.0 as f32;
                    max_vertical_extent = max_vertical_extent.max(screen_space_pos_y.abs() + span_height / 2.0);
                    instances[idx].color = color_for_bucket(node.color_bucket, 1.0).as_rgba_f32();
                    instances[idx].width = config_space_width;
                    instances[idx].position.x = config_space_pos_x + (config_space_width / 2.0);
                    instances[idx].position.y = screen_space_pos_y;
                    instances[idx].vertical_scale = span_height;

                    instances[idx].border_color = Color::Rgba {
                        red: 0.0,
                        green: 0.0,
                        blue: 0.0,
                        alpha: 1.0,      // Fully opaque
                    }.as_rgba_f32();
                    // Hovered state
                    if let Some(&entity) = span_to_text_mapping.identity.get(&node.id) {
                        if let Ok((span, _, _)) = q_span_identities.get_mut(entity) {
                            if span.is_hovered {
                                instances[idx].border_color = Color::Rgba {
                                    red: 1.0,
                                    green: 1.0,
                                    blue: 1.0,
                                    alpha: 1.0,      // Fully opaque
                                }.as_rgba_f32();
                            }
                        }
                    }

                    let text_space_top_left_bound = trace_to_text(vec3(config_space_pos_x, screen_space_pos_y + span_height / 2.0, 0.0));
                    let text_space_bottom_right_bound = trace_to_text(vec3(config_space_pos_x + config_space_width, screen_space_pos_y - span_height / 2.0, 0.0));
                    let text_space_width = text_space_bottom_right_bound.x - text_space_top_left_bound.x;
                    let text_space_height = text_space_top_left_bound.y - text_space_bottom_right_bound.y;

                    let collision_pos = trace_to_text(vec3(config_space_pos_x + config_space_width / 2.0, screen_space_pos_y, 0.0));
                    let collision_width = text_space_width;
                    if HANDLE_COLLISIONS {
                        // // Update or Create collision instances
                        if let Some(&entity) = span_to_text_mapping.identity.get(&node.id) {
                            if let Ok((_, mut collider, mut transform)) = q_span_identities.get_mut(entity) {
                                transform.translation = collision_pos;
                                commands.entity(entity).remove::<Collider>();
                                commands.entity(entity).insert(Collider::cuboid(collision_width/2.0, text_space_height/2.0));
                            }
                        } else {
                            let identity = commands.spawn((
                                IdentifiedSpan {
                                    node_idx: node_idx,
                                    id: node.id.clone(),
                                    is_hovered: false,
                                },
                                TransformBundle::from_transform(Transform::from_translation(collision_pos)),
                                Collider::cuboid(collision_width/2.0, text_space_height/2.0),
                                Sensor,
                                RenderLayers::layer(RENDER_LAYER_TRACE_VIEW),
                                OnTraceScreen
                            )).id();
                            span_to_text_mapping.identity.insert(node.id.clone(), identity);
                        }
                    }

                    // Adjust text position to fit within the bounds of the trace space and viewport
                    let mut text_pos_x = config_space_pos_x;
                    if text_pos_x < left && text_pos_x <= right {
                        text_pos_x = left;
                    }
                    // Convert to text space
                    let text_pos = trace_to_text(vec3(text_pos_x, screen_space_pos_y - (span_height / 4.0), 0.0));

                    // Get target width of the text area
                    let text_area_width = text_space_bottom_right_bound.x - text_pos.x;

                    // Update or Create text instances
                    if RENDER_TEXT {
                        if let Some(&entity) = span_to_text_mapping.spans.get(&node.id) {
                            if let Ok(mut text_bundle) = q_text_elements.get_mut(entity) {
                                text_bundle.2.translation = text_pos;
                                text_bundle.1.size = Vec2::new(text_area_width, if text_area_width < 5.0 { 0.0 } else { 1.0 });
                            }
                        } else {
                            let text = if let TraceEvents::NewSpan {name, location, line, ..} = &node.event {
                                format!("{}: {} ({})", name, location, line)
                            } else {
                                "???".to_string()
                            };
                            let style = text_style.clone();
                            let entity = commands.spawn((
                                Text2dBundle {
                                    text: Text {
                                        sections: vec![TextSection::new(text, style)],
                                        justify: JustifyText::Left,
                                        linebreak_behavior: BreakLineOn::AnyCharacter,
                                        ..default()
                                    },
                                    text_anchor: Anchor::BottomLeft,
                                    transform: Transform::from_translation(text_pos),
                                    text_2d_bounds: Text2dBounds {
                                        size: Vec2::new(text_area_width,  if text_area_width < 5.0 { 0.0 } else { 1.0 }),
                                    },
                                    ..default()
                                },
                                IdentifiedSpan {
                                    node_idx: node_idx,
                                    id: node.id.clone(),
                                    is_hovered: false,
                                },
                                RenderLayers::layer(RENDER_LAYER_TRACE_TEXT),
                                OnTraceScreen
                            )).id();
                            span_to_text_mapping.spans.insert(node.id.clone(), entity);
                        }
                    }

                });
        }
        trace_space.max_vertical_extent = if max_vertical_extent < 1.0 { viewport_height } else { max_vertical_extent };
    }
}


fn enforce_tiled_viewports(
    mut trace_space: ResMut<TraceSpaceViewport>,
    windows: Query<&Window>,
    tree_identities: Res<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    mut main_camera: Query<(&mut Camera, &mut Projection), (With<TraceCameraTraces>, Without<TraceCameraMinimap>, Without<TraceCameraTextAtlas>)>,
    mut text_camera: Query<&mut Camera, (With<TraceCameraTextAtlas>, Without<TraceCameraMinimap>, Without<TraceCameraTraces>)>,
    mut minimap_camera: Query<&mut Camera, (With<TraceCameraMinimap>, Without<TraceCameraTraces>, Without<TraceCameraTextAtlas>)>,
) {
    // TODO: specifically when the graph view is open the traces become misaligned
    let window = windows.single();
    let scale_factor = window.scale_factor() as u32;
    let minimap_offset = MINIMAP_OFFSET * scale_factor;
    let minimap_height = (MINIMAP_HEIGHT * scale_factor);
    let minimap_height_and_offset = MINIMAP_HEIGHT_AND_OFFSET * scale_factor;
    let (mut main_camera , mut projection) = main_camera.single_mut();
    let mut text_camera = text_camera.single_mut();
    let mut minimap_camera = minimap_camera.single_mut();

    if let Some(traces_tile) = tree_identities.traces_tile {
        if let Some(tile) = tree.tree.tiles.get(traces_tile) {
            match tile {
                Tile::Pane(p) => {
                    if &p.nr == &"Traces" {
                        if !tree.tree.active_tiles().contains(&traces_tile) {
                            main_camera.is_active = false;
                            text_camera.is_active = false;
                            minimap_camera.is_active = false;
                            trace_space.is_active = false;
                        } else {
                            trace_space.is_active = true;
                            main_camera.is_active = true;
                            text_camera.is_active = true;
                            minimap_camera.is_active = true;
                            if let Some(r) = p.rect {
                                main_camera.viewport = Some(Viewport {
                                    physical_position: UVec2::new(r.min.x as u32 * scale_factor, (r.min.y as u32 * scale_factor + minimap_height_and_offset)),
                                    physical_size: UVec2::new(
                                        r.width() as u32 * scale_factor,
                                        r.height() as u32 * scale_factor - minimap_height_and_offset,
                                    ),
                                    ..default()
                                });
                                text_camera.viewport = main_camera.viewport.clone();
                                minimap_camera.viewport = Some(Viewport {
                                    physical_position: UVec2::new(r.min.x as u32 * scale_factor, (r.min.y as u32 * scale_factor + minimap_offset)),
                                    physical_size: UVec2::new(
                                        r.width() as u32 * scale_factor,
                                        minimap_height,
                                    ),
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

fn set_camera_viewports(
    windows: Query<&Window>,
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut resize_events: EventReader<WindowResized>,
    mut main_camera: Query<(&mut Camera, &mut Projection), (With<TraceCameraTraces>, Without<TraceCameraMinimap>, Without<TraceCameraTextAtlas>)>,
    mut text_camera: Query<&mut Camera, (With<TraceCameraTextAtlas>, Without<TraceCameraMinimap>, Without<TraceCameraTraces>)>,
    mut minimap_camera: Query<&mut Camera, (With<TraceCameraMinimap>, Without<TraceCameraTraces>, Without<TraceCameraTextAtlas>)>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();
    let minimap_offset = MINIMAP_OFFSET * scale_factor as u32;
    let minimap_height = (MINIMAP_HEIGHT as f32 * scale_factor) as u32;
    let minimap_height_and_offset = MINIMAP_HEIGHT_AND_OFFSET * scale_factor as u32;
    let (mut main_camera , mut projection) = main_camera.single_mut();
    let mut text_camera = text_camera.single_mut();
    let mut minimap_camera = minimap_camera.single_mut();

    // We need to dynamically resize the camera's viewports whenever the window size changes
    // so then each camera always takes up half the screen.
    // A resize_event is sent when the window is first created, allowing us to reuse this system for initial setup.
    for resize_event in resize_events.read() {
        trace_space.vertical_scale = (window.resolution.physical_height() - minimap_height_and_offset) as f32;

        main_camera.viewport = Some(Viewport {
            physical_position: UVec2::new(0, minimap_height_and_offset),
            physical_size: UVec2::new(
                window.resolution.physical_width(),
                window.resolution.physical_height() - minimap_height_and_offset,
            ),
            ..default()
        });
        text_camera.viewport = main_camera.viewport.clone();
        minimap_camera.viewport = Some(Viewport {
            physical_position: UVec2::new(0, minimap_offset),
            physical_size: UVec2::new(
                window.resolution.physical_width(),
                minimap_height,
            ),
            ..default()
        });

    }
}



fn trace_setup(
    windows: Query<&Window>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();

    let minimap_offset = MINIMAP_OFFSET * scale_factor as u32;
    let minimap_height = (MINIMAP_HEIGHT as f32 * scale_factor) as u32;
    let minimap_height_and_offset = MINIMAP_HEIGHT_AND_OFFSET * scale_factor as u32;

    commands.spawn((
        meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
        SpatialBundle::INHERITED_IDENTITY,
        InstanceMaterialData(
            (1..=100)
                .flat_map(|x| 1..=100)
                .map(|_| InstanceData {
                    position: Vec3::new(-100000.0, -10000.0, -2.0),
                    width: 300.0,
                    vertical_scale: 300.0,
                    scale: 1.0,
                    border_color: Color::hsla(360., 1.0, 0.5, 1.0).as_rgba_f32(),
                    color: Color::hsla(360., 1.0, 0.5, 1.0).as_rgba_f32(),
                })
                .collect(),

        ),
        RenderLayers::layer(RENDER_LAYER_TRACE_VIEW),
        // NOTE: Frustum culling is done based on the Aabb of the Mesh and the GlobalTransform.
        // As the cube is at the origin, if its Aabb moves outside the view frustum, all the
        // instanced cubes will be culled.
        // The InstanceMaterialData contains the 'GlobalTransform' information for this custom
        // instancing, and that is not taken into account with the built-in frustum culling.
        // We must disable the built-in frustum culling by adding the `NoFrustumCulling` marker
        // component to avoid incorrect culling.
        NoFrustumCulling,
        OnTraceScreen,
    ));

    // Main trace view camera
    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 2,
                clear_color: ClearColorConfig::Custom(Color::rgba(0.035, 0.035, 0.043, 1.0)),
                viewport: Some(Viewport {
                    physical_position: UVec2::new(0, minimap_height_and_offset),
                    physical_size: UVec2::new(
                        window.resolution.physical_width(),
                        window.resolution.physical_height() - minimap_height_and_offset,
                    ),
                    ..default()
                }),
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 1.0)
                .looking_at(Vec3::ZERO, Vec3::Y),
            projection: OrthographicProjection {
                scale: 1.0,
                scaling_mode: ScalingMode::Fixed {
                    width: CAMERA_SPACE_WIDTH,
                    height: (window.resolution.physical_height() - minimap_height_and_offset) as f32,
                },
                ..default()
            }.into(),
            ..default()
        },
        TraceCameraTraces,
        OnTraceScreen,
        RenderLayers::layer(RENDER_LAYER_TRACE_VIEW)
    ));

    // Text rendering camera
    commands.spawn((
        Camera2dBundle {
            camera: Camera {
                order: 4,
                viewport: Some(Viewport {
                    physical_position: UVec2::new(0, minimap_height_and_offset),
                    physical_size: UVec2::new(
                        window.resolution.physical_width(),
                        window.resolution.physical_height() - minimap_height_and_offset,
                    ),
                    ..default()
                }),
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::ZERO, Vec3::Y),
            ..default()
        },
        OnTraceScreen,
        TraceCameraTextAtlas,
        RenderLayers::layer(RENDER_LAYER_TRACE_TEXT)
    ));

    // Minimap camera
    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 3,
                viewport: Some(Viewport {
                    physical_position: UVec2::new(0, minimap_offset),
                    physical_size: UVec2::new(
                        window.resolution.physical_width(),
                        minimap_height,
                    ),
                    ..default()
                }),
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 1.0)
                .looking_at(Vec3::ZERO, Vec3::Y)
                .with_translation(Vec3::new(0.0, -(minimap_height as f32), 0.0)),
            projection: OrthographicProjection {
                scale: 1.0,
                scaling_mode: ScalingMode::Fixed {
                    width: CAMERA_SPACE_WIDTH,
                    height: (window.resolution.physical_height() - minimap_height) as f32,
                },
                ..default()
            }.into(),
            ..default()
        },
        TraceCameraMinimap,
        OnTraceScreen,
        RenderLayers::from_layers(&[RENDER_LAYER_TRACE_VIEW, RENDER_LAYER_TRACE_MINIMAP])
    ));

    // Minimap viewport indicator
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))).into(),
            material: materials.add(Color::hsla(3.0, 1.0, 1.0, 0.8)),
            transform: Transform::from_xyz(0.0, -50.0, -1.0),
            ..default()
        },
        RenderLayers::layer(RENDER_LAYER_TRACE_MINIMAP),
        MinimapTraceViewportIndicator,
        Collider::cuboid(0.5, 0.5),
        Sensor,
        NoFrustumCulling,
        OnTraceScreen,
    ));

    commands.spawn((CursorWorldCoords(Vec2::ZERO), OnTraceScreen));

    commands.insert_resource(SpanToTextMapping {
        spans: Default::default(),
        identity: Default::default(),
    });

    commands.insert_resource(TraceSpaceViewport {
        x: 0.0,
        y: 0.0,
        horizontal_scale: CAMERA_SPACE_WIDTH,
        vertical_scale: (window.resolution.physical_height() - minimap_height_and_offset) as f32,
        max_vertical_extent: SPAN_HEIGHT * scale_factor,
        is_active: false
    });
    
}

#[derive(Component)]
struct TraceCameraTextAtlas;

#[derive(Component)]
struct TraceCameraTraces;

#[derive(Component)]
struct TraceCameraMinimap;

#[derive(Component)]
struct OnTraceScreen;

pub fn trace_plugin(app: &mut App) {
    app
        .init_resource::<TracesCallTree>()
        .add_plugins((CustomMaterialPlugin, ))
        .add_systems(OnEnter(crate::GameState::Graph), (trace_setup,))
        .add_systems(OnExit(crate::GameState::Graph), despawn_screen::<OnTraceScreen>)
        .add_systems(Update, (
            maintain_call_tree,
            update_trace_space_to_minimap_camera_configuration,
            update_trace_space_to_camera_configuration,
            update_positions,
            mouse_scroll_events,
            my_cursor_system,
            mouse_over_system,
            mouse_pan,
            touchpad_gestures,
            set_camera_viewports,
            enforce_tiled_viewports
        ).run_if(in_state(crate::GameState::Graph)));
}

use std::num::NonZeroU64;
use egui::Order;

#[cfg(test)]
mod tests {
    use super::*;

    fn create_span(id: &str, parent_id: Option<&str>, thread_id: u64, weight: u128) -> TraceEvents {
        TraceEvents::NewSpan {
            id: id.to_string(),
            created_at: Instant::now(),
            thread_id: NonZeroU64::new(thread_id).unwrap(),
            parent_id: parent_id.map(String::from),
            weight,
            name: "test_span".to_string(),
            target: "test_target".to_string(),
            location: "test_location".to_string(),
            line: "1".to_string(),
            execution_id: None,
        }
    }

    fn create_exit(id: &str, weight: u128) -> TraceEvents {
        TraceEvents::Exit(id.to_string(), weight)
    }

    #[test]
    fn test_single_span() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 1);
        assert_eq!(tree.max_render_lane, 0);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 1);
    }

    #[test]
    fn test_nested_spans() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", Some("1"), 1, 10),
            create_exit("2", 50),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 1);
        assert_eq!(tree.max_render_lane, 0);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 2);
    }

    #[test]
    fn test_multiple_threads() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", None, 2, 10),
            create_exit("2", 50),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 1);
        assert_eq!(tree.max_render_lane, 1);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 2);
    }

    #[test]
    fn test_overlapping_spans() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", Some("1"), 1, 10),
            create_span("3", Some("1"), 1, 20),
            create_exit("2", 30),
            create_exit("3", 40),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 1);
        assert_eq!(tree.max_render_lane, 1);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 3);
    }

    #[test]
    fn test_complex_scenario() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", Some("1"), 1, 10),
            create_span("3", Some("2"), 1, 20),
            create_span("4", None, 2, 30),
            create_exit("3", 40),
            create_span("5", Some("2"), 1, 50),
            create_exit("5", 60),
            create_exit("2", 70),
            create_span("6", Some("1"), 1, 80),
            create_exit("4", 90),
            create_exit("6", 95),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 2);
        assert_eq!(tree.max_render_lane, 2);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 6);
    }

    #[test]
    fn test_incomplete_spans() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", Some("1"), 1, 10),
            create_exit("1", 100),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.max_thread_depth, 1);
        assert_eq!(tree.max_render_lane, 0);
        assert_eq!(tree.startpoint, 0);
        assert_eq!(tree.endpoint, 100);
        assert_eq!(tree.graph.node_count(), 2);

        // Check that the incomplete span has a non-zero duration
        let node_2 = tree.graph.node_indices()
            .find(|&idx| tree.graph[idx].id == "2")
            .expect("Node 2 should exist");
        assert!(tree.graph[node_2].total_duration > 0);
    }

    #[test]
    fn test_color_bucket_assignment() {
        let events = vec![
            create_span("1", None, 1, 0),
            create_span("2", None, 2, 10),
            create_span("3", None, 3, 20),
            create_exit("1", 30),
            create_exit("2", 40),
            create_exit("3", 50),
        ];

        let tree = build_call_tree(events, false);

        assert_eq!(tree.graph.node_count(), 3);

        let color_buckets: Vec<f32> = tree.graph.node_indices()
            .map(|idx| tree.graph[idx].color_bucket)
            .collect();

        // Check that color buckets are assigned and in the range [0, 1]
        assert!(color_buckets.iter().all(|&c| c >= 0.0 && c <= 1.0));

        // Check that color buckets are unique (within a small epsilon for float comparison)
        let epsilon = 1e-6;
        for (i, &bucket1) in color_buckets.iter().enumerate() {
            for (j, &bucket2) in color_buckets.iter().enumerate() {
                if i != j {
                    assert!((bucket1 - bucket2).abs() > epsilon,
                            "Color buckets should be unique, but found similar values: {} and {}", bucket1, bucket2);
                }
            }
        }

        // Check that color buckets are evenly distributed
        color_buckets.windows(2).for_each(|w| {
            let diff = w[1] - w[0];
            assert!((diff - 0.5).abs() < epsilon,
                    "Color buckets should be evenly distributed, but found difference: {}", diff);
        });
    }
}