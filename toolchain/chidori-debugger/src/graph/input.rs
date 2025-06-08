//! User input handling and graph navigation systems.
//! 
//! This file manages keyboard and mouse input for navigating the execution graph,
//! including keyboard shortcuts for moving between nodes, mouse interaction detection,
//! and selection state management. It provides the interface for users to interact
//! with and explore the graph visualization.

use crate::application::ChidoriState;
use crate::graph::types::*;
use bevy::prelude::*;
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use crate::bevy_egui::EguiRenderTarget;
use petgraph::visit::{IntoNeighborsDirected};

pub fn keyboard_navigate_graph(
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

pub fn mouse_over_system(
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
    let point = bevy::math::Vec2::new(cursor.0.x, cursor.0.y);
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