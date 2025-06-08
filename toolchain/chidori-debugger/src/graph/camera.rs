//! Camera control and viewport management for the graph visualization.
//! 
//! This file handles all camera-related functionality including pan/zoom controls,
//! minimap synchronization, viewport configuration, mouse and touchpad input handling,
//! and automatic camera positioning to follow selected nodes. It manages both the main
//! graph camera and the minimap camera system.

use crate::application::{ChidoriState, EguiTree, EguiTreeIdentities};
use crate::graph::types::*;
use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::input::touchpad::TouchpadMagnify;
use bevy::math::{vec2, Vec2};
use bevy::prelude::*;
use bevy::render::camera::{ScalingMode, Viewport};
use bevy::window::{PrimaryWindow, WindowResized};
use egui_tiles::Tile;
use crate::{RENDER_LAYER_GRAPH_MINIMAP, RENDER_LAYER_GRAPH_VIEW};

pub fn update_minimap_camera_configuration(
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnGraphScreen>, With<GraphMinimapCamera>)>,
) {
    let (projection, _) = camera.single_mut();
    let (_scale) = match projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { (&mut o.scaling_mode) }
    };
}

pub fn update_trace_space_to_camera_configuration(
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

pub fn set_camera_viewports(
    windows: Query<&Window>,
    mut resize_events: EventReader<WindowResized>,
    mut minimap_camera: Query<&mut Camera, (With<GraphMinimapCamera>, Without<GraphMainCamera>)>,
) {
    let window = windows.single();
    let scale_factor = window.scale_factor();
    let mut minimap_camera = minimap_camera.single_mut();

    for _ in resize_events.read() {
        minimap_camera.viewport = Some(Viewport {
            physical_position: UVec2::new((window.width() * scale_factor) as u32 - (300 * scale_factor as u32), 0),
            physical_size: UVec2::new((300 * scale_factor as u32), (window.height() * scale_factor) as u32),
            ..default()
        });
    }
}

pub fn mouse_pan(
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen>, Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>)>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion_evr: EventReader<MouseMotion>,
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

pub fn mouse_scroll_events(
    graph_resource: Res<GraphResource>,
    mut scroll_evr: EventReader<MouseWheel>,
    mut q_camera: Query<(&mut Projection, &mut Transform, &mut CameraState), (With<OnGraphScreen> , Without<GraphMinimapCamera>, Without<GraphIdxPair>, Without<GraphIdx>, Without<GraphMain2dCamera>)>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    q_mycoords: Query<&CursorWorldCoords, With<OnGraphScreen>>,
    tree_identities: Res<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    q_window: Query<&Window, With<PrimaryWindow>>,
) {
    if !graph_resource.is_active {
        return;
    }

    let window = q_window.single();
    if let Some(graph_tile) = tree_identities.graph_tile {
        if let Some(tile) = tree.tree.tiles.get(graph_tile) {
            match tile {
                Tile::Pane(p) => {
                    if &p.nr == &"Graph" {
                        if let Some(r) = p.rect {
                            if let Some(cursor) = window.cursor_position() {
                                if !r.contains(egui::pos2(cursor.x as f32, cursor.y as f32)) {
                                    return;
                                }
                            }
                        }
                    }
                }
                Tile::Container(_) => {}
            }
        }
    }

    let (projection, mut camera_transform, mut camera_state) = q_camera.single_mut();
    let coords = q_mycoords.single();

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
            camera_transform.translation.y = camera_y;
            camera_transform.translation.x = camera_x;
        } else {
            camera_x -= ev.x * projection.scale;
            camera_y += (ev.y * 2.0) * projection.scale;
            camera_state.state = CameraStateValue::Free(camera_x, camera_y);
        }
    }
}

pub fn touchpad_gestures(
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

pub fn my_cursor_system(
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
        coords.0 = world_position;
    }
}

pub fn camera_follow_selection_head(
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

pub fn enforce_tiled_viewports(
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