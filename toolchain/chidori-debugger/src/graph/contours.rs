//! Graph grouping and contour rendering systems.
//! 
//! This file handles the rendering of graph groupings and contour visualizations
//! that help organize and highlight related nodes in the execution graph. It manages
//! the creation of visual boundaries and grouping indicators that make complex
//! execution flows easier to understand and navigate.

use crate::graph::types::*;
use crate::graph::layout::generate_tree_layout;
use crate::application::ChidoriState;
use crate::accidental::graph_range_collector::{ElementDimensions, RangeCollector, StateRange};
use crate::{RENDER_LAYER_GRAPH_VIEW};
use bevy::prelude::*;
use chidori_core::execution::execution::execution_state::EnclosedState;
use chidori_core::execution::execution::execution_graph::ChronologyId;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use crate::bevy_prototype_lyon::entity::Path;
use crate::bevy_prototype_lyon::path::PathBuilder;
use crate::bevy_prototype_lyon::prelude::ShapeBundle;
use bevy::render::view::RenderLayers;
use petgraph::visit::Topo;

pub fn render_graph_grouping(
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
    let mut topo = Topo::new(&graph_resource.execution_graph);
    while let Some(idx) = topo.next(&graph_resource.execution_graph) {
        if let Some(_) = &graph_resource.execution_graph.node_weight(idx) {
            if let Some((_, n)) = tree_graph.get_from_external_id(&idx.index()) {
                dimensions_map.insert(idx, ElementDimensions {
                    width: n.width as f32,
                    height: n.height as f32,
                    x: n.x as f32,
                    y: -n.y as f32,
                });
            }
        }
    }

    // Second pass: find Open states and collect paths
    let mut topo = Topo::new(&graph_resource.execution_graph);
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