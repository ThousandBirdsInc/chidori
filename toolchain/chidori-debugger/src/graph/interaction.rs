//! Graph node interaction and cursor management.
//! 
//! This file handles interactions between the user and graph nodes, including
//! cursor positioning, selection highlighting, and visual feedback for the current
//! execution head and selected nodes. It manages the visual cursors that indicate
//! the current state and user focus within the graph.

use crate::application::ChidoriState;
use crate::graph::types::*;
use bevy::prelude::*;

pub fn node_cursor_handling(
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
    execution_graph: Res<ChidoriState>,
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